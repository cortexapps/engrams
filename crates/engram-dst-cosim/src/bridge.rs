//! The two-directional boundary bridge (ADR 0098 R-CoSim, rung 1).
//!
//! Only the TRANSPORT between coordinator and host is faked; both sides run
//! real code.
//!
//! * [`CosimHostClient`] implements the coordinator's [`HostClient`] seam
//!   (the same surface `engram-dst`'s toy `SimHostClient` implements) but
//!   routes each verb into the REAL host-agent operations of a shared
//!   [`CosimHost`] — `restore_base_for_session`/`create` build a real
//!   `ChunkedDiskBackend`, `snapshot_begin` drives the REAL eviction capture
//!   leg, `destroy` tears the VM down. This is the coordinator→host
//!   direction.
//!
//! * [`CosimCoordControlPlane`] implements the host-agent's
//!   [`CoordControlPlane`] seam and answers each of its three
//!   decision-feeding calls by invoking the REAL coordinator handler cores
//!   (`live_manifest_publish_core`, `sandbox_ownership_core`,
//!   `sandbox_owner_core`) against a coordinator replica's shared store.
//!   This is the host→coordinator direction — and the one the teardown
//!   reconcile tick reads to decide "is this sandbox still owned?".

use std::collections::HashMap;
use std::net::Ipv4Addr;

use async_trait::async_trait;
use engram_coordinator::api::host_http::{
    live_manifest_publish_core, register_rehydrate_list_core, sandbox_owner_core,
    sandbox_ownership_core, LiveManifestPublishOutcome as CoordPublishOutcome,
    LiveManifestPublishRequest as CoordPublishReq,
};
use engram_coordinator::state::SharedState;
use engram_core::error::SandboxError;
use engram_core::traits::{HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, ExecRequest, ExecStream, SandboxProbe, SandboxSpec,
};
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};
use engram_host_core::{
    CoordControlPlane, CoordError, LiveManifestPublishOutcome, LiveManifestPublishRequest,
    LiveManifestPublishResponse,
};
use engram_sandbox_firecracker::{drive_exec_protocol, ExecRedial};

use crate::host::{build_base_sandbox, SharedHost};

/// The coordinator's per-host [`HostClient`], routing every verb to a shared
/// [`CosimHost`]'s real operations.
pub struct CosimHostClient {
    pub host_id: HostId,
    pub host: SharedHost,
}

impl CosimHostClient {
    pub fn new(host_id: HostId, host: SharedHost) -> Self {
        Self { host_id, host }
    }
}

/// Minimal snapshot metadata (every Option field is `#[serde(default)]`, so
/// an id-only object IS the canonical minimal record).
fn minimal_snapshot_metadata(id: SnapshotId) -> SnapshotMetadata {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "size_bytes": 0,
        "created_at": chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        "image_version": "cosim",
    }))
    .expect("minimal snapshot metadata")
}

/// Build a fresh base sandbox with the host lock held only for the brief
/// synchronous commit — NEVER across the async manifest build (the
/// concurrency rule; see [`crate::host`]).
async fn create_fresh_sandbox(host: &SharedHost) -> SandboxId {
    let (store, cache_root, entropy, pool, device) = {
        let h = host.lock().await;
        (
            h.store_handle(),
            h.cache_root(),
            h.entropy_handle(),
            h.device_pool(),
            h.next_free_device(),
        )
    };
    let (id, base_ref, backend) = build_base_sandbox(store, cache_root, entropy).await;
    // Claim the sandbox's rootfs `/dev/nbdN` on the current-generation pool.
    // Run-step-to-completion scheduling means no two creates race, so this
    // free device is still free here (see the module concurrency note).
    let lease = pool
        .claim(&device)
        .await
        .expect("claim of the fresh sandbox's free device");
    host.lock()
        .await
        .commit_created_sandbox(id, base_ref, backend, device, lease);
    id
}

#[async_trait]
impl HostClient for CosimHostClient {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Ok(create_fresh_sandbox(&self.host).await)
    }

    async fn destroy(&self, id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.host.lock().await.destroy(id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.host.lock().await.list())
    }

    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        let known = self.host.lock().await.contains(id);
        Ok(SandboxProbe {
            known_to_backend: known,
            process_alive: known,
            control_alive: Some(known),
        })
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let (io, severed) = self
            .host
            .lock()
            .await
            .start_exec_transport(id, &cmd)
            .await?;
        let host = self.host.clone();
        let redial = ExecRedial::provided(move || {
            let host = host.clone();
            async move { host.lock().await.redial_exec_transport(id) }
        });
        drive_exec_protocol(id, io, cmd, true, Some(severed), Some(redial)).await
    }

    // `cancel_exec` intentionally retains the HostClient default Unsupported
    // result. The checkpoint-severance composition needs only attach/tail;
    // cancel's real guest verb is covered at the agentd boundary.

    async fn snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        // The composed (non-D5) snapshot path — the eviction scenarios take
        // the `snapshot_begin` split below; this covers a manual snapshot.
        let mut host = self.host.lock().await;
        if !host.contains(id) {
            return Err(SandboxError::NotFound);
        }
        let snap = host
            .snapshot_begin(id)
            .await
            .map_err(SandboxError::Snapshot)?;
        Ok(minimal_snapshot_metadata(snap))
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotId, SandboxError> {
        self.host
            .lock()
            .await
            .snapshot_begin(id)
            .await
            .map_err(SandboxError::Snapshot)
    }

    async fn restore(
        &self,
        _metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        // Resume onto a fresh sandbox (rung 1 asserts the resume round-trip
        // reaches Active, not disk content).
        Ok(create_fresh_sandbox(&self.host).await)
    }

    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: HashMap<String, String>,
        _selected_mounts: Vec<AuxRoDrive>,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        Ok(create_fresh_sandbox(&self.host).await)
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        _agent: AgentSpec,
        _policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        if self.host.lock().await.contains(id) {
            Ok(())
        } else {
            Err(SandboxError::NotFound)
        }
    }

    async fn guest_ip(&self, _id: SandboxId) -> Option<Ipv4Addr> {
        None
    }

    async fn bind_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) {
        self.host.lock().await.bind(session_id, sandbox_id);
    }

    async fn unbind_session(&self, session_id: SessionId) {
        self.host.lock().await.unbind_session(session_id);
    }

    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
        _mode: Option<String>,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// The host-agent's [`CoordControlPlane`], answering each call by driving the
/// REAL coordinator handler cores against a coordinator replica's store.
///
/// It holds the replica's [`SharedState`] directly — the "transport" is a
/// direct function call into the real handler logic, which is exactly the
/// fidelity rung 1 buys (the toy `SimCoordClient` is a BTreeMap; this is the
/// real `update_live_disk_manifest` / `get_session` + terminal-predicate /
/// `session_owning_sandbox` code path).
pub struct CosimCoordControlPlane {
    /// `SharedState` is already `Arc<AppState>` — cheap to clone.
    state: SharedState,
}

impl CosimCoordControlPlane {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    /// The REAL coordinator rehydrate listing for `host_id` (rung 2, #784):
    /// the exact `register` handler core (`register_rehydrate_list_core`) the
    /// host-agent reads on startup to learn which VM-resident survivors to
    /// re-serve. The co-simulated host drives its register-rehydrate leg off
    /// THIS list, so it re-serves precisely the devices the real coordinator
    /// would name — including (or, on the pre-#739 bug, OMITTING) rung-parked
    /// Evicting survivors. A coordinator error degrades to an empty list (the
    /// host runs blind for survivors — the real non-fatal posture).
    pub async fn rehydrate_list(&self, host_id: HostId) -> Vec<SandboxId> {
        register_rehydrate_list_core(&self.state, host_id)
            .await
            .map(|rows| rows.into_iter().map(|r| r.sandbox_id).collect())
            .unwrap_or_default()
    }
}

fn to_coord_error(e: engram_coordinator::ApiError) -> CoordError {
    // The host treats any coordinator error as a transport-class failure
    // (reconcile's `CoordUnreachable` arm → conservatively assume owned).
    CoordError::Transport(format!("{e:?}"))
}

#[async_trait]
impl CoordControlPlane for CosimCoordControlPlane {
    async fn publish_live_manifest(
        &self,
        host_id: HostId,
        req: &LiveManifestPublishRequest,
    ) -> Result<LiveManifestPublishResponse, CoordError> {
        let coord_req = CoordPublishReq {
            session_id: req.session_id,
            sandbox_id: req.sandbox_id,
            manifest_id: req.manifest_id,
            manifest_version: req.manifest_version,
        };
        let outcome = live_manifest_publish_core(&self.state, host_id, &coord_req)
            .await
            .map_err(to_coord_error)?;
        let outcome = match outcome {
            CoordPublishOutcome::Applied => LiveManifestPublishOutcome::Applied,
            CoordPublishOutcome::Stale => LiveManifestPublishOutcome::Stale,
        };
        Ok(LiveManifestPublishResponse { outcome })
    }

    async fn sandbox_ownership(
        &self,
        _host_id: HostId,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<bool, CoordError> {
        sandbox_ownership_core(&self.state, session_id, sandbox_id)
            .await
            .map_err(to_coord_error)
    }

    async fn sandbox_owner(
        &self,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<Option<SessionId>, CoordError> {
        sandbox_owner_core(&self.state, host_id, sandbox_id)
            .await
            .map_err(to_coord_error)
    }
}

// A snapshot row the bridge lands on a completed finalize (mirroring prod's
// heartbeat reconcile). Kept here so the scheduler + tests share one shape.
pub fn recoverable_snapshot_row(
    id: SnapshotId,
    session_id: SessionId,
    host_id: HostId,
    image_version: &str,
    events_cursor: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> SnapshotRecord {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "session_id": session_id,
        "host_id": host_id,
        "image_version": image_version,
        "size_bytes": 0,
        "created_at": now,
        "last_accessed_at": now,
        "recoverable": true,
        "events_cursor": events_cursor,
    }))
    .expect("recoverable snapshot row")
}
