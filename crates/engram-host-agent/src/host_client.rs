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
//! Sandbox methods delegate after the applicable host lifecycle guards.
//! The harness methods (`bind_session`, `unbind_session`,
//! `send_prompt`) delegate to the `HarnessHub`, mapping its
//! `HarnessError` into `SandboxError` so the trait surface stays
//! uniform.

use std::sync::Arc;

use async_trait::async_trait;

use engram_core::error::SandboxError;
use engram_core::traits::{HarnessDial, HarnessSink, HostClient, SandboxBackend, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{
    AgentSpec, ExecRequest, ExecStream, SandboxSpec, SessionFileMetadata, SessionFileSpec,
    SessionFileStream,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{SandboxId, SessionId};

use crate::harness::{HarnessError, HarnessHub};

/// What the idle-eviction capture ([`HostClient::snapshot_begin`]) does
/// with the in-guest harness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdleEvictionHarness {
    /// Capture the live harness and its agent. On resume agentd reattaches
    /// to them, as after a live teleport, so the agent keeps its process
    /// and its in-memory state.
    #[default]
    Keep,
    /// Stop the harness before the capture ([`HarnessHub::drain`]). On
    /// resume agentd spawns a new harness, which starts the agent again
    /// from its on-disk transcript.
    Drain,
}

impl std::fmt::Display for IdleEvictionHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "keep",
            Self::Drain => "drain",
        })
    }
}

impl std::str::FromStr for IdleEvictionHarness {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "keep" => Ok(Self::Keep),
            "drain" => Ok(Self::Drain),
            other => Err(format!(
                "invalid idle-eviction harness mode `{other}`: expected `keep` or `drain`"
            )),
        }
    }
}

#[derive(Clone)]
pub struct LocalHostClient {
    sandbox: Arc<dyn SandboxBackend>,
    harness_hub: Arc<HarnessHub>,
    managed: Option<Arc<crate::managed_policy::ManagedPolicyHost>>,
    boot: Option<Arc<crate::managed_boot::ManagedBootHost>>,
    idle_eviction_harness: IdleEvictionHarness,
}

impl LocalHostClient {
    pub fn new(
        sandbox: Arc<dyn SandboxBackend>,
        harness_hub: Arc<HarnessHub>,
        idle_eviction_harness: IdleEvictionHarness,
    ) -> Self {
        Self {
            sandbox,
            harness_hub,
            managed: None,
            boot: None,
            idle_eviction_harness,
        }
    }

    pub fn with_managed_boot(mut self, boot: Arc<crate::managed_boot::ManagedBootHost>) -> Self {
        self.boot = Some(boot);
        self
    }
    /// The coordinator has disowned this exact sandbox through its heartbeat.
    pub(crate) async fn destroy_tombstoned(&self, id: SandboxId) -> Result<(), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            self.boot()?.destroy_tombstoned(id).await
        } else {
            self.sandbox.destroy(id).await
        }
    }
    fn boot(&self) -> Result<&Arc<crate::managed_boot::ManagedBootHost>, SandboxError> {
        self.boot
            .as_ref()
            .ok_or_else(|| SandboxError::InvalidSpec("managed boot unavailable".into()))
    }
    fn ordinary_only(&self, id: SandboxId) -> Result<(), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return Err(SandboxError::InvalidSpec(
                "managed session requires its current boot owner".into(),
            ));
        }
        Ok(())
    }
    async fn running_or_ordinary(&self, id: SandboxId) -> Result<(), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.guard_running(id).await;
        }
        Ok(())
    }
    pub fn with_managed_policy(
        mut self,
        host: Arc<crate::managed_policy::ManagedPolicyHost>,
    ) -> Self {
        self.managed = Some(host);
        self
    }

    fn check_restore_gate(
        &self,
        fence: SessionFence,
        sandbox: Option<SandboxId>,
    ) -> Result<(), SandboxError> {
        let bindings = self.harness_hub.binding_store();
        let mut managed = bindings.managed_required(fence.session_id);
        if let Some(sandbox) = sandbox {
            managed |= bindings
                .managed_sandbox_required(sandbox)
                .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?;
        }
        if managed {
            return Err(SandboxError::InvalidSpec(
                "managed restore quarantine unavailable".into(),
            ));
        }
        if self.managed.is_some() && fence.session_id.as_uuid().is_nil() {
            return Err(SandboxError::InvalidSpec(
                "restore session identity required".into(),
            ));
        }
        Ok(())
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
        let dir = std::env::temp_dir().join(format!(
            "engram-noop-hub-bindings-{}",
            crate::time_source::unique_path_token()
        ));
        let bindings = crate::bindings::BindingStore::open(dir)
            .expect("open ephemeral binding store for noop hub");
        let hub = Arc::new(HarnessHub::new(
            crate::harness::event_sink_to(|_, _, _, _| async { Ok(()) }),
            bindings,
        ));
        Self::new(sandbox, hub, IdleEvictionHarness::default())
    }
}

#[async_trait]
impl HostClient for LocalHostClient {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        if let Some(boot) = &self.boot {
            boot.ordinary_spec(&spec)?;
        }
        let id = SandboxId::new();
        self.sandbox.create(id, spec).await?;
        Ok(id)
    }

    // ADR 0079: the `fence` params below are checked by the gRPC
    // server's `check_session_epoch` gate BEFORE it delegates here —
    // the local composition is behind the fence, not the fence itself
    // (and in --mode=all there is no second writer to fence out).
    async fn destroy(&self, id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.destroy(id, fence).await;
        }
        self.ordinary_only(id)?;
        // ADR 0073 phase 4: no shell pin to clear — the pin is a PG
        // column stamped by the coordinator's relay and lapses by
        // itself (the issue #219 leak class is gone with the refcount).
        self.sandbox.destroy(id).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        let mut ids = Vec::new();
        for id in self.sandbox.list().await? {
            if !self
                .harness_hub
                .binding_store()
                .managed_sandbox_required(id)
                .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
            {
                ids.push(id);
            }
        }
        if let Some(boot) = &self.boot {
            ids.extend(boot.list()?);
        }
        Ok(ids)
    }

    async fn probe_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.probe_sandbox(id).await
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.exec_stream(id, cmd).await;
        }
        self.ordinary_only(id)?;
        self.sandbox.exec_stream(id, cmd).await
    }

    async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.cancel_exec(id, exec_id).await;
        }
        self.ordinary_only(id)?;
        self.sandbox.cancel_exec(id, exec_id).await
    }

    async fn write_file(
        &self,
        id: SandboxId,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<SessionFileMetadata, SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.write_file(id, spec, bytes).await;
        }
        self.ordinary_only(id)?;
        self.sandbox.write_file(id, spec, bytes).await
    }

    async fn read_file(
        &self,
        id: SandboxId,
        path: String,
    ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
        {
            return self.boot()?.read_file(id, path).await;
        }
        self.ordinary_only(id)?;
        self.sandbox.read_file(id, path).await
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.snapshot(id).await
    }

    async fn snapshot_hold(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.snapshot_hold(id, false).await
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        self.ordinary_only(id)?;
        // The coordinator calls this only for an idle eviction (target
        // `Idle`); live teleport and checkpoints capture through other
        // methods and never drain. A drain is best-effort: a harness that
        // stays attached past the grace is captured live and reattached on
        // resume, so an eviction never waits on a stuck agent.
        if self.idle_eviction_harness == IdleEvictionHarness::Drain
            && !self
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

    async fn migration_presetup(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.migration_presetup(id).await
    }

    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.migration_capture_postcopy(id, export_id).await
    }

    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.migration_drain_wait(id).await
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
        self.ordinary_only(id)?;
        self.sandbox.migration_commit(id, export_id).await
    }

    async fn migration_abort(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.migration_abort(id, export_id).await
    }

    async fn migration_abort_unserved(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<bool, SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.migration_abort_unserved(id, export_id).await
    }

    async fn commit_snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.commit_snapshot(id).await
    }

    async fn abort_snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.ordinary_only(id)?;
        self.sandbox.abort_snapshot(id).await
    }

    async fn restore(
        &self,
        metadata: SnapshotMetadata,
        fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.check_restore_gate(fence, metadata.source_sandbox_id)?;
        if let Some(boot) = &self.boot {
            boot.ordinary_disk(metadata.disk_manifest, None)?;
            boot.ordinary_disk(metadata.swap_manifest, None)?;
        }
        let id = SandboxId::new();
        self.sandbox.restore(id, metadata, None).await?;
        Ok(id)
    }

    async fn materialize_image(
        &self,
        image_uri: &str,
        platform_os: &str,
        platform_arch: &str,
        registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth>,
        min_disk_gib: u32,
        progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
    ) -> Result<engram_core::types::MaterializedImage, SandboxError> {
        self.sandbox
            .materialize_image(
                image_uri,
                platform_os,
                platform_arch,
                registry_auth,
                min_disk_gib,
                progress,
            )
            .await
    }

    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
        fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.check_restore_gate(fence, metadata.source_sandbox_id)?;
        if let Some(boot) = &self.boot {
            boot.ordinary_disk(metadata.disk_manifest, None)?;
            boot.ordinary_disk(metadata.swap_manifest, None)?;
        }
        let id = SandboxId::new();
        self.sandbox
            .restore_base_for_session(id, metadata, session_env, selected_mounts)
            .await?;
        Ok(id)
    }

    async fn read_managed_cold_assets(
        &self,
        request: engram_core::types::managed_assets::ManagedColdAssetsRequest,
    ) -> Result<engram_core::types::managed_assets::ManagedColdAssetsReceipt, SandboxError> {
        self.boot()?.read_cold_assets(request).await
    }
    async fn prepare_managed_boot(
        &self,
        request: engram_core::types::managed_boot::ManagedBootPrepare,
    ) -> Result<engram_core::types::managed_boot::ManagedBootPreparedReceipt, SandboxError> {
        self.boot()?.prepare(request).await
    }
    async fn read_managed_boot(
        &self,
        key: engram_core::types::managed_boot::ManagedBootKey,
    ) -> Result<engram_core::types::managed_boot::ManagedBootHostState, SandboxError> {
        self.boot()?.read(key).await
    }
    async fn finalize_managed_boot(
        &self,
        request: engram_core::types::managed_boot::ManagedBootFinalize,
    ) -> Result<engram_core::types::managed_boot::ManagedBootFinalReceipt, SandboxError> {
        self.boot()?.finalize(request).await
    }
    async fn start_managed_boot(
        &self,
        request: engram_core::types::managed_boot::ManagedBootStart,
    ) -> Result<engram_core::types::managed_boot::ManagedBootHostState, SandboxError> {
        self.boot()?.start(request).await
    }
    async fn capture_managed_boot(
        &self,
        request: engram_core::types::managed_boot::ManagedCaptureRequest,
    ) -> Result<engram_core::types::managed_boot::ManagedCaptureAuthority, SandboxError> {
        self.boot()?.capture(request).await
    }

    async fn read_managed_policy(
        &self,
        target: engram_core::types::managed_deny::ManagedPolicyTarget,
    ) -> Result<Option<engram_core::types::managed_deny::ManagedAppliedState>, SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(target.sandbox_id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
            && self.boot.is_some()
        {
            return self.boot()?.read_policy(target).await;
        }
        self.managed
            .as_ref()
            .ok_or_else(|| SandboxError::InvalidSpec("managed host mode unavailable".into()))?
            .read_applied(&target)
            .await
    }
    async fn apply_managed_policy(
        &self,
        request: engram_core::types::managed_deny::ManagedPolicyApply,
    ) -> Result<engram_core::types::managed_deny::ManagedAppliedState, SandboxError> {
        if self
            .harness_hub
            .binding_store()
            .managed_sandbox_required(request.context.target.sandbox_id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
            && self.boot.is_some()
        {
            return self.boot()?.apply(request).await;
        }
        self.managed
            .as_ref()
            .ok_or_else(|| SandboxError::InvalidSpec("managed host mode unavailable".into()))?
            .compare_and_apply(request)
            .await
    }
    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        let bindings = self.harness_hub.binding_store();
        if bindings
            .managed_sandbox_required(id)
            .map_err(|e| SandboxError::InvalidSpec(e.to_string()))?
            || bindings.managed_required(fence.session_id)
            || policy.managed_deny.is_some()
            || self
                .harness_hub
                .binding_store()
                .managed_required(policy.session_id)
        {
            return Err(SandboxError::InvalidSpec(
                "managed session requires acknowledged boot finalization".into(),
            ));
        }
        // Legacy ordinary callers can still use the explicit unfenced sentinel.
        // Every supplied identity must otherwise agree before any durable mutation.
        if policy.sandbox_id != id
            || policy.session_id.as_uuid().is_nil()
            || (fence != SessionFence::unfenced() && fence.session_id != policy.session_id)
        {
            return Err(SandboxError::InvalidSpec("start identity mismatch".into()));
        }
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
        self.sandbox.notify_session_policy(policy.clone()).await?;
        // ADR 0111: the acked policy is on the node before the ack.
        // A restarted host-agent rebuilds its registry from this file
        // (the reattach-adjacent rebuild pass) with zero coordinator
        // involvement. A persist failure fails the start loudly — an
        // ack whose policy would not survive a roll is the dishonest
        // contract this record exists to remove.
        self.harness_hub
            .persist_egress_policy(&policy)
            .map_err(|e| SandboxError::InvalidSpec(format!("persist egress policy: {e}")))?;
        self.sandbox.start_agent(id, agent).await
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<std::net::Ipv4Addr> {
        if self.ordinary_only(id).is_err() {
            return None;
        }
        self.sandbox
            .guest_endpoints(id)
            .await
            .map(|ep| ep.egress_identity)
    }

    async fn bind_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        binding_epoch: u64,
    ) -> Result<(), engram_core::SandboxError> {
        self.ordinary_only(sandbox_id)?;
        if self
            .harness_hub
            .binding_store()
            .managed_required(session_id)
        {
            return Err(SandboxError::InvalidSpec(
                "managed session requires its boot owner".into(),
            ));
        }
        self.harness_hub
            .bind_session(session_id, sandbox_id, binding_epoch)
            .map_err(|e| engram_core::SandboxError::Vm(format!("bind session: {e}").into()))
    }

    async fn unbind_session(&self, session_id: SessionId) {
        if self
            .harness_hub
            .binding_store()
            .managed_required(session_id)
        {
            return;
        }
        self.harness_hub.unbind_session(session_id);
    }

    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
        mode: Option<String>,
    ) -> Result<(), SandboxError> {
        self.running_or_ordinary(sandbox_id).await?;
        self.harness_hub
            .send_prompt(sandbox_id, prompt_id, text, mode)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn edit_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        self.running_or_ordinary(sandbox_id).await?;
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
        self.running_or_ordinary(sandbox_id).await?;
        self.harness_hub
            .dequeue_queued_prompt(sandbox_id, prompt_id)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn tool_result(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        result_json: String,
    ) -> Result<(), SandboxError> {
        self.running_or_ordinary(sandbox_id).await?;
        self.harness_hub
            .tool_result(sandbox_id, tool_call_id, result_json)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.running_or_ordinary(sandbox_id).await?;
        self.harness_hub
            .interrupt(sandbox_id)
            .await
            .map_err(harness_err_to_sandbox)
    }

    // ADR 0045 Phase F: freeze/unfreeze the microVM in place — pure
    // delegation to the inner backend (no harness involvement).
    async fn pause(&self, sandbox_id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.ordinary_only(sandbox_id)?;
        self.sandbox.pause(sandbox_id).await
    }

    async fn resume(&self, sandbox_id: SandboxId, fence: SessionFence) -> Result<(), SandboxError> {
        self.check_restore_gate(fence, Some(sandbox_id))?;
        self.sandbox.resume(sandbox_id).await
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        self.ordinary_only(sandbox_id)?;
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
        // script.
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
        self.ordinary_only(sandbox_id)?;
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
        self.ordinary_only(sandbox_id)?;
        // ADR 0065: bring up the in-guest browser stack (Xvfb + x11vnc +
        // headful chromium with the CDP debug port) and return the VNC port
        // (+ agentd's optional chromium-CDP liveness warning, issue #569).
        // The orchestrator reaches x11vnc :5900 (and CDP :9222) over the
        // ADR-0066 vsock port relay — the guest binds loopback, agentd dials it.
        self.sandbox.start_browser(sandbox_id).await
    }

    async fn stop_browser(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.ordinary_only(sandbox_id)?;
        // ADR 0065: forward to the inner backend. Teardown is at the snapshot /
        // idle-eviction boundary (the browser is ephemeral, never snapshotted).
        self.sandbox.stop_browser(sandbox_id).await
    }

    async fn start_ide(&self, sandbox_id: SandboxId) -> Result<u16, SandboxError> {
        self.ordinary_only(sandbox_id)?;
        // ADR 0085: bring up the in-guest IDE (code-server) and return its
        // loopback HTTP port. The orchestrator reaches it over the ADR-0066
        // vsock port relay — the guest binds loopback, agentd dials it.
        self.sandbox.start_ide(sandbox_id).await
    }

    async fn stop_ide(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.ordinary_only(sandbox_id)?;
        // ADR 0085: forward to the inner backend. Teardown is at the snapshot /
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
}

fn harness_err_to_sandbox(e: HarnessError) -> SandboxError {
    match e {
        HarnessError::NotAttached => SandboxError::NotFound,
        HarnessError::Io(io) => SandboxError::Io(io),
        HarnessError::SessionMismatch { .. } => SandboxError::InvalidSpec(format!("{e}")),
        HarnessError::Sink(_)
        | HarnessError::FrameEpochAboveAttach { .. }
        | HarnessError::CheckpointAlreadyInFlight
        | HarnessError::CommandTimeout
        | HarnessError::WriterClosed => SandboxError::Vm(format!("{e}").into()),
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use engram_harness_proto::{
        read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessFrame,
    };

    /// Attach a fake harness, run the idle-eviction capture in `mode`, then
    /// interrupt the harness if it is still attached. Returns whether it
    /// stayed attached through the capture and the first command it read.
    async fn idle_capture(mode: IdleEvictionHarness) -> (bool, HarnessCommand) {
        let work = tempfile::tempdir().expect("tempdir");
        let hub = Arc::new(HarnessHub::new(
            crate::harness::event_sink_to(|_, _, _, _| async { Ok(()) }),
            crate::bindings::BindingStore::open(work.path().join("bindings"))
                .expect("binding store"),
        ));
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        let (host_side, harness_side) = tokio::io::duplex(1 << 16);
        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // The fake harness exits after its first command, as the real one
        // does on `Shutdown`.
        let harness = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(harness_side);
            write_msg(
                &mut w,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .expect("attach");
            let ack: HarnessAttachAck = read_msg(&mut r).await.expect("ack");
            assert!(ack.ok, "attach rejected: {:?}", ack.message);
            match read_msg::<_, HarnessFrame>(&mut r).await.expect("frame") {
                HarnessFrame::Command(cmd) => cmd,
                other => panic!("expected a command, got {other:?}"),
            }
        });
        for _ in 0..100 {
            if hub.is_attached(sandbox_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(hub.is_attached(sandbox_id), "harness never attached");

        // The process backend has no split capture: the call returns
        // `InvalidSpec` after the harness step.
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(
            work.path().join("sandboxes"),
        ));
        let client = LocalHostClient::new(backend, hub.clone(), mode);
        let err = client
            .snapshot_begin(sandbox_id, SessionFence::unfenced())
            .await
            .expect_err("process backend has no split capture");
        assert!(matches!(err, SandboxError::InvalidSpec(_)), "{err:?}");

        let attached = hub.is_attached(sandbox_id);
        if attached {
            hub.interrupt(sandbox_id).await.expect("interrupt");
        }
        (attached, harness.await.expect("harness task"))
    }

    #[tokio::test]
    async fn keep_captures_the_live_harness_without_a_shutdown() {
        let (attached, first) = idle_capture(IdleEvictionHarness::Keep).await;
        assert!(attached);
        assert!(matches!(first, HarnessCommand::Interrupt), "{first:?}");
    }

    #[tokio::test]
    async fn drain_shuts_the_harness_down_before_the_capture() {
        let (attached, first) = idle_capture(IdleEvictionHarness::Drain).await;
        assert!(!attached);
        assert!(
            matches!(first, HarnessCommand::Shutdown { .. }),
            "{first:?}"
        );
    }

    #[test]
    fn mode_round_trips_through_its_setting_string() {
        for mode in [IdleEvictionHarness::Keep, IdleEvictionHarness::Drain] {
            assert_eq!(mode.to_string().parse::<IdleEvictionHarness>(), Ok(mode));
        }
        assert!("off".parse::<IdleEvictionHarness>().is_err());
    }
}
