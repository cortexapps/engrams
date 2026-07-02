//! ADR 0018: dead-source session evacuation primitive.
//!
//! `evacuate_dead_source` restores a session onto a peer host from
//! already-recorded artifacts (a snapshot row and/or the live disk
//! manifest) when the source host is gone. Its sole caller is the
//! `evac_resumer` background scanner (driving `Evacuating → Created`),
//! fed by operator drain (ADR 0044 K3). ADR 0045 Phase A retired the
//! reactive NBD-loss / dead-host producers, so this is a drain-only
//! primitive now.
//!
//! The primitive leaves the session at `Created` on the new host: the
//! restored VM has snapshotted memory but a stale harness (its vsock
//! to the source's agentd died with the original sandbox). Callers
//! finish the resume-shape dance by running `start_agent` against the
//! new sandbox + transitioning to `Active`, exactly the way
//! `resume_from_fc_snapshot` in `api/snapshot.rs` finishes a resume.
//! Keeping that step outside the primitive is what lets it be unit-
//! tested against mocks without spinning up SharedState.
//!
//! ADR 0018 commit 12h retired the synchronous *alive-source*
//! `evacuate_to` primitive: the async rework (Evacuating state +
//! scanner) made it dead code. All evac now flows through the
//! state-machine + `evacuate_dead_source` resume path.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::types::evacuation::{EvacLoss, EvacReceipt};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{Session, SessionState};
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::{MetaError, SandboxError};

use crate::host_registry::HostRegistry;
use crate::placement::{PickError, ScheduleContext};

/// Errors specific to the evacuation primitive. Wraps the upstream
/// `SandboxError` / `MetaError` so callers can distinguish which step
/// failed for retry / telemetry decisions.
#[derive(Debug)]
pub enum EvacError {
    RestoreFailed(SandboxError),
    Rebind(MetaError),
    /// No recoverable state to restore from. Dead-source path returns
    /// this when both `snapshot` and `session.live_disk_manifest` are
    /// `None`. Caller routes to `HostLost → Dead`.
    NoRecoverableState,
    /// ADR 0028 Fix B: the session is disk-only recoverable (a live
    /// disk manifest exists but no usable snapshot) and the caller
    /// couldn't supply a cold-boot spec — typically because the
    /// session's image is no longer enabled, so there's no manifest
    /// to derive boot resources from. Structural: retrying won't fix
    /// it; the caller routes to `Idle` (re-enable the image, then
    /// `/resume` recovers via the same cold-boot path).
    ColdBootUnavailable(String),
    /// No host could accept the relocate (no capacity, or no host
    /// with the image prefetched). Caller logs + retries later or
    /// routes to `HostLost → Dead`.
    NoTargetAvailable(PickError),
}

impl EvacError {
    /// ADR 0028 Fix B fail-fast guard: structural errors can never be
    /// fixed by retrying — burning the resumer's 20-attempt budget on
    /// them (the `cf4d4afd` incident's ~3 min of `RestoreFailed`
    /// churn) just delays the honest terminal state.
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::NoRecoverableState | Self::ColdBootUnavailable(_)
        )
    }
}

impl std::fmt::Display for EvacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RestoreFailed(e) => write!(f, "target-side restore failed: {e}"),
            Self::Rebind(e) => write!(f, "PG rebind failed: {e}"),
            Self::NoRecoverableState => {
                write!(
                    f,
                    "no snapshot or live disk manifest — session cannot be evacuated"
                )
            }
            Self::ColdBootUnavailable(reason) => {
                write!(
                    f,
                    "disk-only recoverable but no cold-boot spec available: {reason}"
                )
            }
            Self::NoTargetAvailable(e) => write!(f, "no host could accept the relocate: {e:?}"),
        }
    }
}

impl std::error::Error for EvacError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoRecoverableState
            | Self::ColdBootUnavailable(_)
            | Self::NoTargetAvailable(_) => None,
            Self::RestoreFailed(e) => Some(e),
            Self::Rebind(e) => Some(e),
        }
    }
}

/// ADR 0028 Fix B: derive the disk-only recovery's cold-boot
/// `SandboxSpec` from the session's enabled image (manifest-derived
/// resources, env, bundles — `api::sessions::cold_boot_spec`).
/// `None` when the image row is gone/unreadable or its manifest
/// doesn't parse — callers pass that through and
/// `evacuate_dead_source` fails structurally (`ColdBootUnavailable`)
/// only if the recovery actually needed it.
pub async fn resolve_cold_boot_spec(
    meta: &Arc<dyn MetadataStore>,
    session: &Session,
) -> Option<engram_core::types::sandbox::SandboxSpec> {
    let enabled = match meta.get_enabled_image(&session.image).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(
                session_id = %session.id,
                image = %session.image,
                "cold-boot spec: image is not enabled; disk-only recovery unavailable",
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session.id,
                image = %session.image,
                error = %e,
                "cold-boot spec: enabled-image lookup failed",
            );
            return None;
        }
    };
    let manifest: engram_core::types::ImageManifest = match toml::from_str(&enabled.manifest_toml) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                session_id = %session.id,
                image = %session.image,
                error = %e,
                "cold-boot spec: manifest_toml parse failed",
            );
            return None;
        }
    };
    // ADR 0057: disk-only recovery rebuilds the session's own egress network
    // from its persisted policy (the manifest no longer carries network).
    let network = match meta.get_session_integration_policy(session.id).await {
        Ok(Some(json)) => engram_core::types::IntegrationPolicy::parse(&json)
            .ok()
            .flatten()
            .map(|p| p.network)
            .unwrap_or_default(),
        _ => Default::default(),
    };
    Some(crate::api::sessions::cold_boot_spec(
        &session.image,
        &manifest,
        None,
        network,
    ))
}

/// Pick the disk manifest the target should restore from. Mirrors the
/// `effective_resume_disk_manifest` semantics in `api/snapshot.rs`:
/// when both live and snapshot manifests exist, prefer the live one
/// only if it's a strictly newer version of the same manifest_id
/// lineage; otherwise the snapshot wins (different lineage means we
/// trust the (memory, disk) pair the snapshot captured together).
fn pick_evac_disk_manifest(
    live: Option<ManifestRef>,
    snapshot: Option<ManifestRef>,
) -> Option<ManifestRef> {
    match (live, snapshot) {
        (None, snap) => snap,
        (Some(l), None) => Some(l),
        (Some(l), Some(s)) => {
            if l.manifest_id == s.manifest_id && l.version > s.version {
                Some(l)
            } else {
                Some(s)
            }
        }
    }
}

/// Mechanics for **dead-source** evacuation. Driven by the
/// `evac_resumer` scanner for `Evacuating` sessions (produced by
/// operator drain, ADR 0044 K3) — restores from existing artifacts
/// because the source backend is unreachable, so a fresh source-side
/// snapshot isn't possible. (ADR 0045 Phase A retired the reactive
/// dead-host / NBD-loss producers that used to feed this.)
///
/// Restores from existing artifacts, rung-aware (ADR 0028 recovery
/// ladder):
///
/// - **Rung 1 — coherent checkpoint** (`snapshot.memory_manifest`
///   present): restore memory + the checkpoint's OWN
///   `snapshot.disk_manifest`. Deliberately NOT the newer
///   `live_disk_manifest`: the restored RAM describes the checkpoint's
///   disk, so pairing it with a later disk version is incoherent. This
///   IS the "rewind the disk to the checkpoint" step — post-checkpoint
///   continuous-sync deltas are discarded for coherence. `EvacLoss::None`.
/// - **Rung 2 — cold boot** (no memory): a fresh kernel mounts the
///   `pick_evac_disk_manifest(live, snapshot)` disk (live-wins-when-
///   newer — newest files, no coherence constraint since there's no
///   RAM to disagree). `EvacLoss::Memory { reason: "source-dead-no-snapshot" }`.
///
/// Failure modes:
///
/// - `snapshot.is_none() && session.live_disk_manifest.is_none()` →
///   `NoRecoverableState`. Caller drives `HostLost → Dead`.
/// - Target pick fails (no capacity, image not ready) →
///   `NoTargetAvailable`. Caller logs + may retry; nothing changed.
/// - Target-side restore fails → `RestoreFailed`. Caller may retry
///   against a different host; nothing changed.
/// - PG rebind fails after restore → new sandbox is up on target
///   without a PG row pointing at it (a small orphan window). Idle
///   evictor / host-agent restart sweep reaps.
///
/// Leaves the session at `Created` on the new host. Caller is
/// responsible for the start_agent + Active transition.
pub async fn evacuate_dead_source(
    registry: &Arc<HostRegistry>,
    meta: &Arc<dyn MetadataStore>,
    session: Session,
    snapshot: Option<SnapshotRecord>,
    // ADR 0028 Fix B: the disk-only recovery's boot shape (from
    // `api::sessions::cold_boot_spec`, manifest-derived resources).
    // `None` is fine when a memory snapshot exists; when the session
    // is disk-only recoverable and this is `None`, the call fails
    // structurally with `ColdBootUnavailable`.
    cold_boot_spec: Option<engram_core::types::sandbox::SandboxSpec>,
    // ADR 0045 Phase F (teleport): when `Some`, place onto this exact
    // host instead of the capacity-ranked pick (operator-pinned
    // destination). `None` keeps the standard any-peer policy.
    require_host: Option<engram_core::HostId>,
    // ADR 0045 C2 (E2B fold, origin affinity): soft preference for this
    // host in the capacity-ranked pick — tier-2, below snapshot
    // affinity, loses to exclude/draining/capacity (host_registry's
    // standard precedence). The disk-only RESUME path passes the
    // session's ORIGIN host (its chunk cache + base shm are warm
    // there); movers (drain, dead-host) pass `None` — they are moving
    // AWAY by definition.
    prefer_host: Option<engram_core::HostId>,
) -> Result<EvacReceipt, EvacError> {
    let session_id = session.id;
    let old_sandbox_id = session.sandbox_id;

    let memory_manifest = snapshot.as_ref().and_then(|s| s.memory_manifest);
    let snapshot_disk = snapshot.as_ref().and_then(|s| s.disk_manifest);

    // ADR 0028 rung-aware disk pick — the coherence rule. With a
    // coherent memory snapshot (rung 1), the restored RAM's page
    // cache + mounted-fs metadata describe the checkpoint's OWN disk;
    // pairing it with the newer continuous-sync `live_disk_manifest`
    // is incoherent (corruption — Defect B in miniature). So rung 1
    // uses the snapshot's disk verbatim. Only rung 2 (cold boot, no
    // memory — a fresh kernel mounts whatever it's given) takes the
    // live-wins preference.
    let disk_manifest = if memory_manifest.is_some() {
        snapshot_disk
    } else {
        pick_evac_disk_manifest(session.live_disk_manifest, snapshot_disk)
    };

    if disk_manifest.is_none() && memory_manifest.is_none() {
        return Err(EvacError::NoRecoverableState);
    }

    let (loss, reason) = if memory_manifest.is_some() {
        (EvacLoss::None, "")
    } else {
        (
            EvacLoss::Memory {
                reason: "source-dead-no-snapshot".into(),
            },
            "source-dead-no-snapshot",
        )
    };
    let _ = reason; // structured-log placeholder; metric label lives on `loss.as_str()`.

    // ADR 0028 Fix B: validate the disk-only branch's prerequisites
    // BEFORE the host pick. Structural failures (no cold-boot spec)
    // must not hide behind transient ones (`NoTargetAvailable`) —
    // otherwise a capacity blip masks an unrecoverable session and
    // the resumer retries something retrying can't fix.
    let cold_boot = if memory_manifest.is_some() {
        None
    } else {
        let disk = disk_manifest
            .expect("disk-only branch requires a disk manifest (NoRecoverableState guards above)");
        let mut spec = cold_boot_spec.ok_or_else(|| {
            EvacError::ColdBootUnavailable(format!(
                "session {session_id} has a live disk manifest ({disk}) but no \
                 cold-boot spec — is its image still enabled?"
            ))
        })?;
        spec.rootfs_manifest = Some(disk);
        Some(spec)
    };

    let (image_repo, image_tag) = engram_core::types::session::split_image_ref(&session.image);
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: snapshot.as_ref().map(|s| s.id),
        memory_mib: None,
        cpu_budget_vcpus: None,
        // Target-selection: image-cache-warm preference is a future
        // refinement (defer when we add zone tagging to HostState).
        // Today we accept any host that can take the work, but never
        // the source host (exclude_host) — set to the prior owner via
        // `session.host_id` so a drain doesn't relocate back onto the
        // host being drained. For the dead-source path the source is
        // already unregistered; exclude_host is defensive.
        required_image_digest: None,
        exclude_host: session.host_id,
        prefer_host,
        // ADR 0068: same pairing the resume path uses — a memory
        // manifest needs the FC UFFD substrate, and (when known) the
        // target must match the snapshot's capture-time
        // `fc_snapshot_version` exactly.
        caps: crate::placement::CapabilityRequirements {
            needs_uffd_substrate: memory_manifest.is_some(),
            fc_snapshot_version: snapshot
                .as_ref()
                .and_then(|s| s.fc_snapshot_version.clone()),
        },
    };

    // Split pick + restore so picker errors and backend errors keep
    // distinct typing — picker failures are `NoTargetAvailable`
    // (operator action: free capacity or wait for image prefetch);
    // backend failures are `RestoreFailed` (retry against another
    // host or surface to user). ADR 0045 Phase F: an operator-pinned
    // teleport target bypasses capacity ranking and places on that
    // exact host (still excluding the source); a bad pin retries then
    // falls back to Idle rather than silently landing elsewhere.
    let (target_host, target_backend) = match require_host {
        Some(host) => {
            crate::placement::pick_specific_host(meta.as_ref(), registry, host, session.host_id)
                .await
                .map_err(EvacError::NoTargetAvailable)?
        }
        None => crate::placement::pick_for_session(meta.as_ref(), registry, &ctx)
            .await
            .map_err(EvacError::NoTargetAvailable)?,
    };

    let new_sandbox_id = match cold_boot {
        // ADR 0028 Fix B — rung 2: no coherent memory snapshot, but
        // the continuous-sync disk manifest is current. A full-FC
        // restore is structurally impossible here (no state.bin, no
        // sidecar — the pre-Fix-B code minted a nil-blob-key
        // SnapshotId and burned the resumer's whole budget on
        // "manifest.json: No such file or directory"). And pairing
        // the image's BASE memory with this *evolved* disk would be
        // incoherent (restored RAM's page cache + mounted-fs metadata
        // describe the base disk → corruption). The coherent recovery
        // is a FRESH KERNEL BOOT mounting the recovered rootfs: the
        // live manifest is a full rootfs lineage, so the host
        // NBD-attaches it and boots clean. On-disk work survives;
        // in-RAM context does not (`EvacLoss::Memory`).
        Some(spec) => target_backend
            .create(spec)
            .await
            .map_err(EvacError::RestoreFailed)?,
        // Rung-1-shaped recovery: a coherent memory snapshot exists.
        // Lift the snapshot row's fields verbatim (id, size,
        // image_version) and DERIVE the portable blob keys from the
        // snapshot_id. The keys are deterministic functions of the id
        // (`snapshots/<id>/{state.bin,sidecar.json}`), so we can
        // rebuild them at restore time without needing dedicated
        // columns on the snapshot row.
        //
        // ADR 0018 commit 12 (async evac): the source records the
        // snapshot in PG before the scanner picks the session up,
        // which loses the `state_blob_key` / `sidecar_blob_key` that
        // `PooledBackend::snapshot` populated on the in-memory
        // metadata. Deriving them here is the canonical fix — same
        // shape as the matching `materialize_state_if_missing` helper
        // that consumes them on the target. Without these, the target
        // host's FC `restore()` errors with "manifest.json: No such
        // file or directory" because nothing materialised the sidecar.
        None => {
            let s = snapshot
                .as_ref()
                .expect("memory_manifest implies a snapshot row");
            // ADR 0045 D4: best-effort image-base canonical ref (shared
            // per-image base shm on the target); None falls back to
            // canonical == session.
            let base_memory_manifest = match meta.get_enabled_image(&session.image).await {
                Ok(Some(img)) => match img.base_snapshot_id {
                    Some(bid) => meta
                        .get_snapshot(bid)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|b| b.memory_manifest),
                    None => None,
                },
                _ => None,
            };
            let metadata = SnapshotMetadata {
                migration_source: None,
                id: s.id,
                size_bytes: s.size_bytes,
                created_at: s.created_at,
                image_version: s.image_version.clone(),
                disk_manifest,
                memory_manifest,
                base_memory_manifest,
                source_sandbox_id: None,
                state_blob_key: Some(engram_chunk_store::snapshot_blob::state_blob_key(s.id)),
                sidecar_blob_key: Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(s.id)),
                rootfs_blob_key: None,
                working_set_blob_key: None,
                // ADR 0035: evac-dest restore is resume-flavored — keep the
                // pinned generations; the target host materializes them.
                aux_bundles: s.aux_bundles.clone(),
            };
            target_backend
                .restore(metadata)
                .await
                .map_err(EvacError::RestoreFailed)?
        }
    };

    // Routing cache: invalidate the stale source binding (the source
    // host is dead, so this is usually already gone from
    // `host_registry.unregister`, but defensive). Insert the new.
    if let Some(old) = old_sandbox_id {
        registry.invalidate_sandbox(old);
    }
    registry.record_sandbox_owner(new_sandbox_id, target_host);

    // PG rebind. The `evac_resumer` scanner has the session at
    // `Evacuating` (operator drain flipped Active → Evacuating), so we
    // drive Evacuating → Created here. (`transition_session` enforces
    // the legality table; both Evacuating → Created and the legacy
    // HostLost → Created are legal edges.)
    meta.assign_session_host(session_id, Some(target_host))
        .await
        .map_err(EvacError::Rebind)?;
    meta.assign_session_sandbox(session_id, Some(new_sandbox_id))
        .await
        .map_err(EvacError::Rebind)?;
    meta.transition_session(session_id, SessionState::Created)
        .await
        .map_err(EvacError::Rebind)?;

    Ok(EvacReceipt {
        new_host_id: target_host,
        new_sandbox_id,
        loss,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use engram_core::traits::{HarnessDial, HostClient};
    use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::session::{Session, SessionMode, SessionState};
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::{HostId, SandboxId, SessionId};
    use parking_lot::Mutex as PlMutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every call made against it; deterministic returns so
    /// the orchestration's call ordering is observable in tests.
    /// Snapshot returns a SnapshotMetadata with deterministic UUID;
    /// restore returns the pre-staged `next_restore_id`.
    #[derive(Default)]
    struct FakeBackend {
        next_restore_id: PlMutex<Option<SandboxId>>,
        fail_snapshot: AtomicUsize, // 0=ok, 1=fail
        fail_restore: AtomicUsize,
        /// ADR 0028 Fix B: the spec the disk-only cold-boot branch
        /// passed to `create()`, for assertions.
        last_create_spec: PlMutex<Option<SandboxSpec>>,
        /// ADR 0028 rung-1: the metadata the warm branch passed to
        /// `restore()`, for the coherence-rule assertions.
        last_restore_metadata: PlMutex<Option<SnapshotMetadata>>,
    }

    impl FakeBackend {
        fn set_restore_id(&self, id: SandboxId) {
            *self.next_restore_id.lock() = Some(id);
        }

        fn last_restore_metadata(&self) -> Option<SnapshotMetadata> {
            self.last_restore_metadata.lock().clone()
        }

        fn last_create_spec(&self) -> Option<SandboxSpec> {
            self.last_create_spec.lock().clone()
        }
    }

    #[async_trait]
    impl HostClient for FakeBackend {
        async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            *self.last_create_spec.lock() = Some(spec);
            // Reuse the restore-id knob so disk-only tests can pin
            // the expected sandbox id; fresh id otherwise (match-based
            // to dodge the unwrap_or_default lint — a nil-UUID default
            // would mask test bugs).
            Ok(match *self.next_restore_id.lock() {
                Some(id) => id,
                None => SandboxId::new(),
            })
        }
        async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(vec![])
        }
        async fn probe_sandbox(
            &self,
            _id: SandboxId,
        ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
            unimplemented!()
        }
        async fn exec_stream(
            &self,
            _id: SandboxId,
            _cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            unreachable!()
        }
        async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            if self.fail_snapshot.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr(
                    "snapshot failed".into(),
                ))));
            }
            Ok(SnapshotMetadata {
                id: engram_core::SnapshotId::new(),
                size_bytes: 1024,
                created_at: chrono::Utc::now(),
                image_version: "test".into(),
                base_memory_manifest: None,
                migration_source: None,
                disk_manifest: None,
                memory_manifest: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
            })
        }
        async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn restore(&self, md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            *self.last_restore_metadata.lock() = Some(md);
            if self.fail_restore.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr(
                    "restore failed".into(),
                ))));
            }
            // FakeBackend tests always preload a restore id; the
            // fallback is just defensive against a misconfigured test.
            // Lifted out of unwrap_or_else / unwrap_or to dodge clippy's
            // unwrap_or_default lint (Default would mint a nil UUID,
            // which would mask test bugs vs. a fresh id flagging them).
            Ok(match *self.next_restore_id.lock() {
                Some(id) => id,
                None => SandboxId::new(),
            })
        }
        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: engram_core::types::sandbox::AgentSpec,
            _policy: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unreachable!("evac primitive does not call start_agent")
        }
        async fn apply_egress_policy(
            &self,
            _policy: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
            None
        }
        async fn bind_session(&self, _session_id: SessionId, _sandbox_id: SandboxId) {}
        async fn unbind_session(&self, _session_id: SessionId) {}
        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _prompt_id: String,
            _text: String,
        ) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn acquire_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn release_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unreachable!()
        }
        fn harness_dial(&self) -> HarnessDial {
            HarnessDial::Vsock
        }
    }

    #[derive(Debug)]
    struct SimpleErr(String);
    impl std::fmt::Display for SimpleErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for SimpleErr {}

    /// Minimal MetadataStore that records every state-affecting call.
    /// Stores one session row keyed by SessionId; the evac orchestration
    /// reads it via `get_session` and updates via the assign_* /
    /// transition_session calls.
    #[derive(Default)]
    struct FakeMeta {
        sessions: PlMutex<HashMap<SessionId, Session>>,
        sandbox_to_session: PlMutex<HashMap<SandboxId, (HostId, SessionState)>>,
        /// ADR 0047: placement reads host rows — tests stage
        /// schedulable hosts here via [`Self::add_ready_host`].
        hosts: PlMutex<Vec<engram_core::types::host::HostRecord>>,
    }

    impl FakeMeta {
        /// Stage a fresh-heartbeat `ready` host row so the PG-read
        /// picker can schedule onto the registered backend.
        fn add_ready_host(&self, id: HostId) {
            self.hosts
                .lock()
                .push(engram_core::types::host::HostRecord {
                    id,
                    hostname: format!("fake-{id}"),
                    cloud_metadata: Default::default(),
                    capacity: engram_core::types::host::HostCapacity {
                        total_gb: 0,
                        used_gb: 0,
                        total_mib: 16_384,
                        used_mib: 0,
                        running_sandboxes: 0,
                    },
                    utilization: Default::default(),
                    status: engram_core::types::host::HostStatus::Ready,
                    last_heartbeat_at: chrono::Utc::now(),
                    host_addr: None,
                    ready_images: Vec::new(),
                    local_snapshots: Vec::new(),
                    current_bundles: Vec::new(),
                    cordoned: false,
                    total_vcpus: 0,
                    wire_version: 0,
                    capabilities: engram_core::types::host::HostCapabilities::default(),
                });
        }

        fn install_session(&self, sess: Session) {
            if let (Some(_), Some(sb)) = (sess.host_id, sess.sandbox_id) {
                self.sandbox_to_session
                    .lock()
                    .insert(sb, (sess.host_id.unwrap(), sess.status));
            }
            self.sessions.lock().insert(sess.id, sess);
        }
        fn session(&self, id: SessionId) -> Session {
            self.sessions.lock().get(&id).cloned().unwrap()
        }
    }

    #[async_trait]
    impl MetadataStore for FakeMeta {
        async fn create_session(
            &self,
            _: engram_core::types::session::SessionSpec,
        ) -> Result<SessionId, MetaError> {
            unreachable!()
        }
        async fn create_session_created(
            &self,
            _: SessionId,
            _: engram_core::types::session::SessionSpec,
            _: HostId,
            _: SandboxId,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
            self.sessions
                .lock()
                .get(&id)
                .cloned()
                .ok_or(MetaError::NotFound)
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(self.sessions.lock().values().cloned().collect())
        }
        async fn transition_session(
            &self,
            id: SessionId,
            target: SessionState,
        ) -> Result<SessionState, MetaError> {
            let mut guard = self.sessions.lock();
            let s = guard.get_mut(&id).ok_or(MetaError::NotFound)?;
            let prev = s.status;
            s.status
                .try_transition_to(target)
                .map_err(|e| MetaError::Conflict(e.to_string()))?;
            s.status = target;
            Ok(prev)
        }
        async fn assign_session_host(
            &self,
            id: SessionId,
            host_id: Option<HostId>,
        ) -> Result<(), MetaError> {
            self.sessions
                .lock()
                .get_mut(&id)
                .ok_or(MetaError::NotFound)?
                .host_id = host_id;
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            id: SessionId,
            sandbox_id: Option<SandboxId>,
        ) -> Result<(), MetaError> {
            self.sessions
                .lock()
                .get_mut(&id)
                .ok_or(MetaError::NotFound)?
                .sandbox_id = sandbox_id;
            Ok(())
        }
        async fn host_for_sandbox(
            &self,
            sandbox_id: SandboxId,
        ) -> Result<Option<(HostId, SessionState)>, MetaError> {
            Ok(self.sandbox_to_session.lock().get(&sandbox_id).copied())
        }
        async fn upsert_host(
            &self,
            _: engram_core::types::host::HostRecord,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_active_hosts(
            &self,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, MetaError> {
            Ok(self.hosts.lock().clone())
        }
        async fn set_host_status(
            &self,
            _: HostId,
            _: engram_core::types::host::HostStatus,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::host::HostHeartbeat,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn set_host_cordoned(&self, _: HostId, _: bool) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_stale_hosts(
            &self,
            _: u64,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(
            &self,
            _: engram_core::types::snapshot::SnapshotRecord,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_snapshots_for_session(
            &self,
            _: SessionId,
        ) -> Result<Vec<engram_core::types::snapshot::SnapshotRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn latest_snapshot_for_session(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::snapshot::SnapshotRecord>, MetaError> {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _: SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _: SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<engram_core::types::event::PersistedEvent>, MetaError> {
            Ok(Vec::new())
        }
        async fn insert_artifact(
            &self,
            _: uuid::Uuid,
            _: SessionId,
            _: &str,
            _: &str,
            _: i64,
            _: Option<&str>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_artifact(
            &self,
            _: SessionId,
            _: uuid::Uuid,
        ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
            Ok(None)
        }
        async fn artifact_usage(&self, _: SessionId) -> Result<(i64, i64), MetaError> {
            Ok((0, 0))
        }
        async fn upsert_registry_credential(
            &self,
            _: engram_core::types::registry::RegistryCredential,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_registry_credentials(
            &self,
        ) -> Result<Vec<engram_core::types::registry::RegistryCredential>, MetaError> {
            Ok(Vec::new())
        }
        async fn registry_credential_for_host(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::RegistryCredential>, MetaError> {
            Ok(None)
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
            unreachable!()
        }
        // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
        async fn upsert_enabled_image(
            &self,
            _: engram_core::types::registry::EnabledImage,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_enabled_images(
            &self,
        ) -> Result<Vec<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_enabled_image(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn get_enabled_image_any(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn soft_delete_enabled_image(
            &self,
            _: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
            unreachable!()
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn upsert_session_secrets(
            &self,
            _: engram_core::types::registry::SessionSecrets,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn get_session_secrets(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::registry::SessionSecrets>, MetaError> {
            Ok(None)
        }
        async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
            unreachable!()
        }
    }

    fn make_session(host: HostId, sandbox: SandboxId, status: SessionState) -> Session {
        Session {
            id: SessionId::new(),
            status,
            host_id: Some(host),
            sandbox_id: Some(sandbox),
            image: "ghcr.io/test/img:t".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    fn make_snapshot_for(
        session_id: SessionId,
        disk: Option<ManifestRef>,
        memory: Option<ManifestRef>,
    ) -> SnapshotRecord {
        SnapshotRecord {
            id: engram_core::SnapshotId::new(),
            session_id: Some(session_id),
            host_id: None,
            image_version: "test".into(),
            size_bytes: 1024,
            created_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            disk_manifest: disk,
            memory_manifest: memory,
            recoverable: true,
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        }
    }

    fn fake_manifest(id: u128, version: u64) -> ManifestRef {
        ManifestRef {
            manifest_id: uuid::Uuid::from_u128(id),
            version,
        }
    }

    /// Helper: build a HostRegistry + register one target host with
    /// fresh capacity. Returns (registry, target_host, target_backend).
    fn build_registry_with_target(
        meta: Arc<FakeMeta>,
    ) -> (Arc<HostRegistry>, HostId, Arc<FakeBackend>) {
        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let target_host = HostId::new();
        let target_be = Arc::new(FakeBackend::default());
        registry.register(target_host, target_be.clone());
        meta.add_ready_host(target_host);
        // pick_for_session's capacity fallback path requires a fresh
        // host with no draining flag — register() sets defaults that
        // suffice.
        (registry, target_host, target_be)
    }

    /// ADR 0028 rung-1 COHERENCE RULE: a coherent memory snapshot is
    /// present AND the live disk manifest is strictly newer (same
    /// lineage). The restore MUST use the checkpoint's OWN disk — NOT
    /// the newer live one — because the restored RAM describes the
    /// checkpoint's disk; pairing it with later disk deltas corrupts.
    /// This is the "rewind the disk to the checkpoint" step.
    ///
    /// (Pre-Fix-A this arm used live-wins and was named
    /// `..._uses_live_disk_when_newer` — that encoded the bug.)
    #[tokio::test]
    async fn evac_dead_source_rung1_uses_checkpoint_disk_not_newer_live() {
        let meta = Arc::new(FakeMeta::default());
        let lineage = 0xABCD;
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        // Live disk is a strictly-newer version of the SAME lineage —
        // exactly the case pick_evac_disk_manifest would prefer.
        session.live_disk_manifest = Some(fake_manifest(lineage, 5));
        let session_id = session.id;
        meta.install_session(session.clone());

        let checkpoint_disk = fake_manifest(lineage, 3);
        let memory = fake_manifest(lineage + 1, 1);
        let snapshot = make_snapshot_for(session_id, Some(checkpoint_disk), Some(memory));

        let (registry, target_host, target_be) = build_registry_with_target(meta.clone());
        let new_sandbox = SandboxId::new();
        target_be.set_restore_id(new_sandbox);

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            Some(snapshot),
            None,
            None,
            None,
        )
        .await
        .expect("rung-1 happy path");
        assert_eq!(receipt.new_host_id, target_host);
        assert_eq!(receipt.new_sandbox_id, new_sandbox);
        assert_eq!(receipt.loss, EvacLoss::None);

        // The coherence assertion: warm restore, checkpoint's own disk,
        // checkpoint's memory — the newer live disk (v5) is ignored.
        let md = target_be
            .last_restore_metadata()
            .expect("rung-1 must go through restore(), not create()");
        assert_eq!(
            md.disk_manifest,
            Some(checkpoint_disk),
            "rung-1 must pair memory with the checkpoint's OWN disk (v3), \
             not the newer live disk (v5)",
        );
        assert_eq!(md.memory_manifest, Some(memory));
        assert!(
            target_be.last_create_spec().is_none(),
            "rung-1 is a restore, never a cold-boot create",
        );

        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Created);
        assert_eq!(updated.host_id, Some(target_host));
        assert_eq!(updated.sandbox_id, Some(new_sandbox));
    }

    /// Arm 2: snapshot present, no live_disk → restore uses the
    /// snapshot's disk + memory. Loss=None.
    #[tokio::test]
    async fn evac_dead_source_uses_snapshot_when_no_live_disk() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = None;
        let session_id = session.id;
        meta.install_session(session.clone());

        let snapshot = make_snapshot_for(
            session_id,
            Some(fake_manifest(0x1234, 7)),
            Some(fake_manifest(0x5678, 7)),
        );

        let (registry, _target_host, target_be) = build_registry_with_target(meta.clone());
        target_be.set_restore_id(SandboxId::new());

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            Some(snapshot),
            None,
            None,
            None,
        )
        .await
        .expect("snapshot-only happy path");
        assert_eq!(receipt.loss, EvacLoss::None);
    }

    /// ADR 0045 C2 (E2B fold, origin affinity): the disk-only RESUME
    /// path threads the session's origin host as a soft preference —
    /// its NBD chunk cache + base shm are warm there. Two otherwise
    /// equal hosts: the pick lands on the preferred one. (Precedence —
    /// snapshot affinity above, exclude/draining/capacity vetoes — is
    /// pinned by host_registry's own prefer_host tests.)
    #[tokio::test]
    async fn evac_prefers_the_origin_host_when_passed() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = None;
        // Mirrors resume_disk_only_cold_boot: host_id cleared (so the
        // origin is NOT excluded), origin threaded as prefer_host.
        session.host_id = None;
        let session_id = session.id;
        meta.install_session(session.clone());
        let snapshot = make_snapshot_for(
            session_id,
            Some(fake_manifest(0x1111, 1)),
            Some(fake_manifest(0x2222, 1)),
        );

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let origin = HostId::new();
        let other = HostId::new();
        let origin_be = Arc::new(FakeBackend::default());
        let other_be = Arc::new(FakeBackend::default());
        registry.register(other, other_be.clone());
        registry.register(origin, origin_be.clone());
        meta.add_ready_host(other);
        meta.add_ready_host(origin);
        origin_be.set_restore_id(SandboxId::new());
        other_be.set_restore_id(SandboxId::new());

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            Some(snapshot),
            None,
            None,
            Some(origin),
        )
        .await
        .expect("origin-affinity happy path");
        assert_eq!(
            receipt.new_host_id, origin,
            "with equal capacity the origin preference must win the pick"
        );
    }

    /// A plausible cold-boot spec, the shape `resolve_cold_boot_spec`
    /// would derive from an enabled image.
    fn test_cold_boot_spec() -> SandboxSpec {
        SandboxSpec {
            image: "ghcr.io/test/img:t".into(),
            rootfs_source: None,
            image_uri: Some("ghcr.io/test/img:t".into()),
            rootfs_manifest: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 2 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 4096 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 20 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    /// Arm 3 (ADR 0028 Fix B): no snapshot, live_disk present →
    /// disk-only COLD BOOT — `create()` with the session's live
    /// manifest as the rootfs override, NOT a structurally-impossible
    /// `restore()`. Loss=Memory{reason}.
    #[tokio::test]
    async fn evac_dead_source_disk_only_cold_boots_with_memory_loss() {
        let meta = Arc::new(FakeMeta::default());
        let live = fake_manifest(0xCAFE, 9);
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(live);
        let session_id = session.id;
        meta.install_session(session.clone());

        let (registry, _target_host, target_be) = build_registry_with_target(meta.clone());
        let new_sandbox = SandboxId::new();
        target_be.set_restore_id(new_sandbox);

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
            Some(test_cold_boot_spec()),
            None,
            None,
        )
        .await
        .expect("disk-only cold-boot happy path");
        match &receipt.loss {
            EvacLoss::Memory { reason } => {
                assert_eq!(reason, "source-dead-no-snapshot");
            }
            other => panic!("expected Memory loss, got {other:?}"),
        }
        assert_eq!(receipt.new_sandbox_id, new_sandbox);

        // The recovery went through create() with the live manifest as
        // the rootfs override — the fresh-kernel-boot shape.
        let spec = target_be
            .last_create_spec()
            .expect("disk-only recovery must call create(), not restore()");
        assert_eq!(spec.rootfs_manifest, Some(live));

        // PG was rebound through HostLost → Created.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Created);
    }

    /// Arm 3b (ADR 0028 Fix B): disk-only recoverable but no cold-boot
    /// spec (image no longer enabled) → structural ColdBootUnavailable,
    /// PG untouched. The resumer fail-fasts this to Idle instead of
    /// burning its budget.
    #[tokio::test]
    async fn evac_dead_source_disk_only_without_spec_is_structural() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(fake_manifest(0xCAFE, 9));
        let session_id = session.id;
        meta.install_session(session.clone());

        let (registry, _target_host, _target_be) = build_registry_with_target(meta.clone());

        let result = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
            None,
            None,
            None,
        )
        .await;
        match &result {
            Err(e @ EvacError::ColdBootUnavailable(_)) => assert!(e.is_structural()),
            other => panic!("expected ColdBootUnavailable, got {other:?}"),
        }
        assert_eq!(meta.session(session_id).status, SessionState::HostLost);
    }

    /// Arm 4: no snapshot, no live_disk → NoRecoverableState. Caller
    /// (dead_host.rs) routes this to HostLost → Dead.
    #[tokio::test]
    async fn evac_dead_source_no_state_errors_no_recoverable() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = None;
        let session_id = session.id;
        meta.install_session(session.clone());

        let (registry, _target_host, _target_be) = build_registry_with_target(meta.clone());

        let result = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(result, Err(EvacError::NoRecoverableState)));

        // PG untouched at HostLost.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::HostLost);
    }

    /// No registered target host → NoTargetAvailable. Caller logs +
    /// may retry.
    #[tokio::test]
    async fn evac_dead_source_no_target_returns_no_target_available() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(fake_manifest(0x1, 1));
        meta.install_session(session.clone());

        // Empty registry — no hosts to pick.
        let registry = Arc::new(HostRegistry::new(meta.clone()));

        let result = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
            Some(test_cold_boot_spec()),
            None,
            None,
        )
        .await;
        assert!(matches!(result, Err(EvacError::NoTargetAvailable(_))));
    }

    /// pick_evac_disk_manifest semantics: same lineage → newer wins;
    /// different lineage → snapshot wins; None handling. Cheap pure-
    /// function test, mirrors the api/snapshot.rs effective_resume
    /// tests.
    #[test]
    fn pick_disk_prefers_live_only_when_newer_same_lineage() {
        let live = fake_manifest(0xAA, 5);
        let snap = fake_manifest(0xAA, 3);
        assert_eq!(pick_evac_disk_manifest(Some(live), Some(snap)), Some(live));
    }

    #[test]
    fn pick_disk_falls_back_to_snapshot_on_different_lineage() {
        let live = fake_manifest(0xAA, 99);
        let snap = fake_manifest(0xBB, 1);
        assert_eq!(pick_evac_disk_manifest(Some(live), Some(snap)), Some(snap),);
    }

    #[test]
    fn pick_disk_handles_either_none() {
        let snap = fake_manifest(0xAA, 1);
        assert_eq!(pick_evac_disk_manifest(None, Some(snap)), Some(snap));
        let live = fake_manifest(0xBB, 1);
        assert_eq!(pick_evac_disk_manifest(Some(live), None), Some(live));
        assert_eq!(pick_evac_disk_manifest(None, None), None);
    }
}
