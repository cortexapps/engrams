//! Durable teleport driver (ADR 0123 B).
//!
//! Teleport payloads are either `{source_host, reason}` for planned admission,
//! or `{teleport_id}` for a move that has already been admitted. The row's
//! phase is the recovery cursor. An op step marker never replaces that row.

use crate::error::ApiError;
use crate::session_ops::{OpCtx, OpOutcome};
use crate::state::{SessionEvent, SharedState};
use engram_core::types::host::{HostStatus, RetirementGrant};
use engram_core::types::session_op::{OpKind, OpState};
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::types::teleport::TeleportSettle;
use engram_core::types::teleport::*;
use engram_core::types::{BindingDisposition, SessionState};
use engram_core::{HostId, SandboxError, SessionId};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct TeleportConfig {
    pub poll_interval: Duration,
    pub max_open_per_dest: u32,
}
impl Default for TeleportConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            max_open_per_dest: 1,
        }
    }
}
pub fn spawn(cfg: TeleportConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(cfg.poll_interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            timer.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error=%e,"teleport scan failed");
            }
        }
    })
}
pub async fn run_once(
    cfg: &TeleportConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error>> {
    for host in state.services.meta.list_retiring_hosts().await? {
        plan_host_teleports_with_limit(
            state,
            host.id,
            TeleportReason::RetireHost,
            cfg.max_open_per_dest,
        )
        .await;
        if matches!(
            state
                .services
                .meta
                .grant_host_retirement(host.id, state.services.clock.now_utc())
                .await?,
            RetirementGrant::Granted
        ) {
            tracing::info!(host_id=%host.id,"host retirement granted");
            metrics::counter!("engram_host_retirement_granted_total").increment(1);
        }
    }
    for row in state.services.meta.list_open_teleports().await? {
        if !state
            .services
            .meta
            .op_pending_exists(row.session_id, OpKind::Teleport)
            .await?
        {
            enqueue_deferred(
                state,
                row.session_id,
                OpKind::Teleport,
                serde_json::json!({"teleport_id":row.id,"max_open_per_dest":cfg.max_open_per_dest}),
                Some(&format!("teleport:{}", row.id)),
            )
            .await?;
        }
    }
    Ok(())
}
#[derive(Default)]
pub(crate) struct PlanReport {
    pub planned: Vec<SessionId>,
    pub descended: Vec<SessionId>,
    pub skipped: u32,
}
pub(crate) async fn plan_host_teleports(
    state: &SharedState,
    host: HostId,
    reason: TeleportReason,
) -> PlanReport {
    plan_host_teleports_with_limit(
        state,
        host,
        reason,
        TeleportConfig::default().max_open_per_dest,
    )
    .await
}
async fn plan_host_teleports_with_limit(
    state: &SharedState,
    host: HostId,
    reason: TeleportReason,
    max_open_per_dest: u32,
) -> PlanReport {
    let mut report = PlanReport::default();
    let assignments = match state
        .services
        .meta
        .list_resident_sandbox_assignments_on_host(host)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(%host,error=%e,"teleport planning failed");
            return report;
        }
    };
    for (id, _, status) in assignments {
        let result = match status {
            SessionState::Active => enqueue_deferred(
                state,
                id,
                OpKind::Teleport,
                serde_json::json!({"source_host":host,"reason":reason,"max_open_per_dest":max_open_per_dest}),
                Some(&format!("teleport:{host}:{id}")),
            )
            .await
            .map(|_| {
                report.planned.push(id);
            }),
            SessionState::Parked => match state.services.meta.get_session(id).await {
                Ok(s) => crate::idle_evictor::descend_parked_session(state, &s, "teleport")
                    .await
                    .map(|done| {
                        if done {
                            report.descended.push(id)
                        } else {
                            report.skipped += 1
                        }
                    }),
                Err(e) => Err(e),
            },
            _ => {
                report.skipped += 1;
                continue;
            }
        };
        if let Err(e) = result {
            report.skipped += 1;
            tracing::warn!(session_id=%id,error=%e,"teleport planning failed");
        }
    }
    report
}

async fn enqueue_deferred(
    state: &SharedState,
    id: SessionId,
    kind: OpKind,
    payload: serde_json::Value,
    key: Option<&str>,
) -> Result<(), engram_core::MetaError> {
    if let engram_core::types::session_op::EnqueueOutcome::Claimed(op) =
        crate::session_ops::enqueue_claim(state, id, kind, payload, key).await?
    {
        state
            .services
            .meta
            .op_requeue_with_backoff(
                op.id,
                op.epoch.expect("claimed epoch"),
                Duration::ZERO,
                "teleport scheduled",
            )
            .await?;
    }
    Ok(())
}

pub(crate) async fn admit_for_rpc(
    state: &SharedState,
    id: SessionId,
    target: Option<HostId>,
) -> Result<TeleportRow, ApiError> {
    let claim=crate::session_ops::OpClaim::try_acquire(state,id,OpKind::Teleport,serde_json::json!({"source_host":state.services.meta.get_session(id).await?.host_id,"reason":TeleportReason::Ui})).await?
        .ok_or_else(||ApiError::Conflict("busy_lane".into()))?;
    let admitted = steps::admit(&claim.as_ctx(), target, TeleportReason::Ui).await;
    match admitted {
        Ok(TeleportAdmitOutcome::Admitted(row)) => {
            // Queue the durable row while the exclusive admission claim still owns the lane.
            enqueue_deferred(
                state,
                id,
                OpKind::Teleport,
                serde_json::json!({"teleport_id":row.id}),
                Some(&format!("teleport:{}", row.id)),
            )
            .await?;
            claim.finish(OpState::Done, None).await;
            Ok(*row)
        }
        other => {
            claim.finish(OpState::Done, None).await;
            match other {
                Ok(TeleportAdmitOutcome::NoFit) => Err(ApiError::Conflict("no_fit".into())),
                Ok(TeleportAdmitOutcome::SessionNotActive(_)) => {
                    Err(ApiError::Conflict("not_active".into()))
                }
                Ok(TeleportAdmitOutcome::Fenced) => Err(ApiError::Conflict("busy_lane".into())),
                Err(e) => Err(e),
                Ok(TeleportAdmitOutcome::Admitted(_)) => unreachable!(),
            }
        }
    }
}

pub(crate) async fn drive(ctx: &OpCtx<'_>) -> OpOutcome {
    match drive_inner(ctx).await {
        Ok(out) => out,
        Err(e) => OpOutcome::Retry(e.to_string()),
    }
}
async fn drive_inner(ctx: &OpCtx<'_>) -> Result<OpOutcome, ApiError> {
    loop {
        let Some(row) = ctx
            .state
            .services
            .meta
            .open_teleport_for_session(ctx.op.session_id)
            .await?
        else {
            if ctx.op.payload.get("teleport_id").is_some() {
                return Ok(OpOutcome::Done);
            }
            let s = ctx
                .state
                .services
                .meta
                .get_session(ctx.op.session_id)
                .await?;
            let source: Option<HostId> = ctx
                .op
                .payload
                .get("source_host")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            if source.is_none() || source != s.host_id {
                return Ok(OpOutcome::Done);
            }
            let reason = ctx
                .op
                .payload
                .get("reason")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or(TeleportReason::RetireHost);
            if !matches!(
                steps::admit(ctx, None, reason).await?,
                TeleportAdmitOutcome::Admitted(_)
            ) {
                return Ok(OpOutcome::Done);
            }
            continue;
        };
        if !ctx.step(row.phase.as_str()).await {
            return Ok(OpOutcome::Done);
        }
        let session = ctx.state.services.meta.get_session(row.session_id).await?;
        // A crash can occur between the fenced settle and the terminal row CAS.
        if session.sandbox_id.is_none()
            && matches!(session.status, SessionState::Idle | SessionState::Dead)
        {
            let error = match row.phase {
                TeleportPhase::RollingBack => "source_lost_during_rollback",
                TeleportPhase::Attached => "peer_lost",
                _ => "dest_lost_after_blackout",
            };
            ctx.state
                .services
                .meta
                .teleport_settle(
                    row.id,
                    ctx.epoch,
                    TeleportSettle {
                        error: error.into(),
                        session: None,
                        entomb_source: false,
                    },
                )
                .await?;
            return Ok(OpOutcome::Done);
        }
        let outcome = match row.phase {
            TeleportPhase::Admitted => steps::capture(ctx, &row).await?,
            TeleportPhase::Captured => steps::restore(ctx, &row).await?,
            TeleportPhase::Restored => steps::commit(ctx, &row).await?,
            TeleportPhase::Committed => steps::attach(ctx, &row).await?,
            TeleportPhase::Attached => steps::release(ctx, &row).await?,
            TeleportPhase::RollingBack => steps::rollback(ctx, &row).await?,
            TeleportPhase::Done | TeleportPhase::Aborted | TeleportPhase::Failed => {
                return Ok(OpOutcome::Done)
            }
        };
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
    }
}
async fn advance(
    ctx: &OpCtx<'_>,
    row: &TeleportRow,
    to: TeleportPhase,
    patch: TeleportPatch,
) -> Result<Option<OpOutcome>, ApiError> {
    Ok((!ctx
        .state
        .services
        .meta
        .teleport_advance(row.id, row.phase, to, patch, ctx.epoch)
        .await?)
        .then_some(OpOutcome::Done))
}
async fn rollback_begin(
    ctx: &OpCtx<'_>,
    row: &TeleportRow,
    error: String,
) -> Result<Option<OpOutcome>, ApiError> {
    advance(
        ctx,
        row,
        TeleportPhase::RollingBack,
        TeleportPatch {
            error: Some(error),
            ..Default::default()
        },
    )
    .await
}
/// A dead or retired host, or one whose row was deleted: nothing can
/// answer for the sandbox any more.
async fn source_gone(ctx: &OpCtx<'_>, row: &TeleportRow) -> Result<bool, ApiError> {
    Ok(ctx
        .state
        .services
        .meta
        .get_host(row.source_host_id)
        .await?
        .is_none_or(|h| matches!(h.status, HostStatus::Dead | HostStatus::Retired)))
}

mod steps {
    use super::*;
    async fn live_metadata(
        _ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<SnapshotMetadata, ApiError> {
        serde_json::from_value(
            row.live_payload
                .clone()
                .ok_or_else(|| ApiError::Internal("live teleport has no restore payload".into()))?,
        )
        .map_err(|e| ApiError::Internal(format!("live restore payload: {e}")))
    }
    async fn capture_live(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        let state = ctx.state;
        let host = match state.host_registry.backend_for(row.source_host_id).await {
            Ok(host) => host,
            Err(e) => return rollback_begin(ctx, row, e.to_string()).await,
        };
        if row.export_id.is_none() {
            let presetup = match host
                .migration_presetup(row.source_sandbox_id, ctx.fence())
                .await
            {
                Ok(p) => p,
                Err(SandboxError::InvalidSpec(_)) => {
                    return advance(
                        ctx,
                        row,
                        TeleportPhase::Admitted,
                        TeleportPatch {
                            kind: Some(TeleportKind::Snapshot),
                            ..Default::default()
                        },
                    )
                    .await
                }
                Err(e) => return rollback_begin(ctx, row, e.to_string()).await,
            };
            let session = state.services.meta.get_session(row.session_id).await?;
            let durable = state
                .services
                .meta
                .latest_snapshot_for_session(row.session_id)
                .await?;
            let source_addr = state
                .services
                .meta
                .get_host(row.source_host_id)
                .await?
                .and_then(|h| h.host_addr)
                .ok_or_else(|| ApiError::Unavailable("source host has no address".into()))?;
            let hostpart = source_addr
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            let hostname = hostpart.rsplit_once(':').map_or(hostpart, |(h, _)| h);
            let peer_addr = format!("{hostname}:{}", presetup.peer_port);
            let metadata = SnapshotMetadata {
                id: engram_core::SnapshotId::from(state.services.entropy.uuid()),
                size_bytes: durable.as_ref().map_or(0, |s| s.size_bytes),
                created_at: state.services.clock.now_utc(),
                image_version: durable
                    .as_ref()
                    .map_or_else(|| session.image.clone(), |s| s.image_version.clone()),
                disk_manifest: presetup.disk_manifest_ref,
                memory_manifest: Some(presetup.memory_manifest_ref),
                base_memory_manifest: crate::api::snapshot::base_memory_manifest_for_image(
                    state,
                    &session.image,
                )
                .await,
                migration_source: Some(engram_core::types::snapshot::MigrationSourceInfo {
                    export_id: presetup.export_id.clone(),
                    source_addr,
                    memory_manifest_json: presetup.memory_manifest_json,
                    disk_manifest_json: Vec::new(),
                    memory_manifest_ref: presetup.memory_manifest_ref,
                    disk_manifest_ref: presetup.disk_manifest_ref.ok_or_else(|| {
                        ApiError::Internal("live export has no disk manifest".into())
                    })?,
                    new_memory_chunk_hashes: Vec::new(),
                    new_disk_chunk_hashes: Vec::new(),
                    hot_chunks: presetup.hot_chunks,
                    post_copy: true,
                    peer_addr: Some(peer_addr),
                    peer_token: Some(presetup.peer_token),
                    sidecar_json: presetup.sidecar_json,
                }),
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: durable.map(|s| s.aux_bundles).unwrap_or_default(),
                paused_at: None,
                peer_hints: Vec::new(),
            };
            return advance(
                ctx,
                row,
                TeleportPhase::Admitted,
                TeleportPatch {
                    export_id: Some(presetup.export_id),
                    live_payload: Some(
                        serde_json::to_value(metadata)
                            .map_err(|e| ApiError::Internal(e.to_string()))?,
                    ),
                    ..Default::default()
                },
            )
            .await;
        }
        let metadata = live_metadata(ctx, row).await?;
        let dest = state.host_registry.backend_for(row.dest_host_id).await?;
        let fence = ctx.fence();
        let mut task = tokio::task::JoinSet::new();
        task.spawn(async move { dest.restore(metadata, fence).await });
        let captured = host
            .migration_capture_postcopy(
                row.source_sandbox_id,
                row.export_id.as_deref().expect("export set"),
                ctx.fence(),
            )
            .await;
        if let Err(e) = captured {
            task.abort_all();
            let error = if matches!(e, SandboxError::NotFound | SandboxError::AlreadyExists)
                || e.to_string().contains("presetup")
            {
                "live_export_lost".into()
            } else {
                e.to_string()
            };
            return rollback_begin(ctx, row, error).await;
        }
        if let Some(out) =
            advance(ctx, row, TeleportPhase::Captured, TeleportPatch::default()).await?
        {
            task.abort_all();
            return Ok(Some(out));
        }
        let mut captured_row = row.clone();
        captured_row.phase = TeleportPhase::Captured;
        let restored = task
            .join_next()
            .await
            .expect("restore task")
            .map_err(|e| ApiError::Unavailable(e.to_string()))?;
        finish_live_restore(ctx, &captured_row, restored).await
    }
    async fn finish_live_restore(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
        result: Result<engram_core::SandboxId, SandboxError>,
    ) -> Result<Option<OpOutcome>, ApiError> {
        match result {
            Ok(id) => {
                ctx.state
                    .host_registry
                    .record_sandbox_owner(id, row.dest_host_id);
                advance(
                    ctx,
                    row,
                    TeleportPhase::Restored,
                    TeleportPatch {
                        dest_sandbox_id: Some(id),
                        ..Default::default()
                    },
                )
                .await
            }
            Err(SandboxError::NotFound | SandboxError::AlreadyExists) => {
                rollback_begin(ctx, row, "live_export_lost".into()).await
            }
            Err(e) if e.to_string().contains("postcopy-never-loaded") => {
                ctx.state
                    .host_registry
                    .backend_for(row.source_host_id)
                    .await?
                    .migration_abort(
                        row.source_sandbox_id,
                        row.export_id.as_deref().expect("live export"),
                        ctx.fence(),
                    )
                    .await?;
                rollback_begin(ctx, row, e.to_string()).await
            }
            Err(e) if ctx.op.attempts <= 3 => Ok(Some(OpOutcome::Retry(e.to_string()))),
            Err(e) => fail_move(ctx, row, &format!("dest_lost_after_blackout: {e}"), false).await,
        }
    }
    async fn destination_gone(ctx: &OpCtx<'_>, row: &TeleportRow) -> Result<bool, ApiError> {
        Ok(ctx
            .state
            .services
            .meta
            .get_host(row.dest_host_id)
            .await?
            .is_none_or(|h| matches!(h.status, HostStatus::Dead | HostStatus::Retired)))
    }
    async fn destroy_destination(ctx: &OpCtx<'_>, row: &TeleportRow) -> Result<(), ApiError> {
        if let Some(sandbox) = row.dest_sandbox_id {
            if !destination_gone(ctx, row).await? {
                match ctx
                    .state
                    .host_registry
                    .backend_for(row.dest_host_id)
                    .await?
                    .destroy(sandbox, ctx.fence())
                    .await
                {
                    Ok(()) | Err(SandboxError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            ctx.state
                .services
                .meta
                .record_sandbox_tombstone(row.dest_host_id, sandbox, Some(row.session_id))
                .await?;
        }
        Ok(())
    }
    async fn fail_move(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
        error: &str,
        allow_snapshot: bool,
    ) -> Result<Option<OpOutcome>, ApiError> {
        let state = ctx.state;
        destroy_destination(ctx, row).await?;
        let session = state.services.meta.get_session(row.session_id).await?;
        // Release runs after attach. Return through Evacuating before detaching.
        if session.status == SessionState::Active {
            crate::session_ops::transition_with_fence(
                state,
                row.session_id,
                ctx.fence(),
                SessionState::Evacuating,
                BindingDisposition::Retain,
            )
            .await?;
        }
        let target = settle_target(ctx, row, &session, allow_snapshot).await?;
        // The source tombstone, the session's terminal state, and the
        // failed row land in ONE fenced transaction: a driver that lost the
        // lane writes none of them.
        state
            .services
            .meta
            .teleport_settle(
                row.id,
                ctx.epoch,
                TeleportSettle {
                    error: error.into(),
                    session: Some(target),
                    entomb_source: true,
                },
            )
            .await?;
        Ok(Some(OpOutcome::Done))
    }
    /// Where a session rests when its move fails: the dead-host predicate
    /// (`recovery_target`: Idle with a recoverable memory snapshot or a live
    /// disk manifest, Dead otherwise), except that a session parked at
    /// Created by a deterministic spawn failure has no Dead edge and rests
    /// at Failed. `allow_snapshot = false` discards the snapshot evidence
    /// (the move itself produced it and it is not trusted).
    async fn settle_target(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
        session: &engram_core::types::Session,
        allow_snapshot: bool,
    ) -> Result<SessionState, ApiError> {
        let has_recoverable_snapshot = allow_snapshot
            && ctx
                .state
                .services
                .meta
                .latest_snapshot_for_session(row.session_id)
                .await?
                .is_some_and(|s| s.recoverable);
        let target = crate::dead_host::recovery_target(
            has_recoverable_snapshot,
            session.live_disk_manifest.is_some(),
        );
        Ok(match (session.status, target) {
            (SessionState::Created, SessionState::Dead) => SessionState::Failed,
            (_, t) => t,
        })
    }

    pub(super) async fn admit(
        ctx: &OpCtx<'_>,
        target: Option<HostId>,
        reason: TeleportReason,
    ) -> Result<TeleportAdmitOutcome, ApiError> {
        let state = ctx.state;
        let session = state.services.meta.get_session(ctx.op.session_id).await?;
        if session.status != SessionState::Active {
            return Ok(TeleportAdmitOutcome::SessionNotActive(session.status));
        }
        let (mem, cpu) =
            crate::boot_materializer::resolve_resume_budget(&state.services.meta, &session)
                .await
                .ok_or_else(|| ApiError::Unavailable("teleport budget unavailable".into()))?;
        let caps = crate::placement::CapabilityRequirements {
            needs_uffd_substrate: true,
            fc_snapshot_version: match session.host_id {
                Some(h) => state.services.meta.fc_snapshot_version_for_host(h).await?,
                None => None,
            },
        };
        if let Some(target) = target {
            let host = state
                .services
                .meta
                .get_host(target)
                .await?
                .ok_or_else(|| ApiError::Conflict("no_fit".into()))?;
            if host.cordoned {
                return Err(ApiError::Conflict("target_cordoned".into()));
            }
            if crate::placement::host_meets_capabilities(&host, &caps).is_err() {
                return Err(ApiError::Conflict("target_lacks_capability".into()));
            }
        }
        let context = crate::placement::ScheduleContext {
            repo: &session.image,
            image_version: "",
            snapshot_host: None,
            memory_mib: Some(mem),
            cpu_budget_vcpus: Some(cpu),
            required_image_digest: None,
            exclude_host: session.host_id,
            prefer_host: None,
            caps,
            prefer_bundles: &[],
        };
        let candidates = crate::placement::candidates_for(
            state.services.meta.as_ref(),
            &context,
            state.services.clock.now_utc(),
        )
        .await
        .map_err(|e| ApiError::Unavailable(format!("placement: {e:?}")))?
        .hosts;
        if target.is_some_and(|h| !candidates.contains(&h)) {
            return Ok(TeleportAdmitOutcome::NoFit);
        }
        let live_host = |h: &engram_core::types::host::HostRecord| {
            h.capabilities.backend == "firecracker"
                && matches!(
                    h.capabilities.base_shm_tmpfs,
                    engram_core::types::host::CapStatus::Ok(_)
                )
                && matches!(
                    h.capabilities.uffd_minor_shmem,
                    engram_core::types::host::CapStatus::Ok(_)
                )
                && matches!(
                    h.capabilities.nbd,
                    engram_core::types::host::CapStatus::Ok(_)
                )
        };
        let hosts = state.services.meta.list_active_hosts().await?;
        let live_capable = session
            .host_id
            .is_some_and(|source| hosts.iter().any(|h| h.id == source && live_host(h)))
            && candidates
                .iter()
                .filter(|id| target.is_none_or(|t| t == **id))
                .all(|id| hosts.iter().any(|h| h.id == *id && live_host(h)));
        state
            .services
            .meta
            .teleport_admit(TeleportAdmitRequest {
                id: engram_core::TeleportId::from(state.services.entropy.uuid()),
                session_id: session.id,
                reason,
                epoch: ctx.epoch,
                candidates,
                pinned_dest: target,
                mem_budget_mib: i64::from(mem),
                cpu_budget_vcpus: i64::from(cpu),
                max_open_per_dest: ctx
                    .op
                    .payload
                    .get("max_open_per_dest")
                    .and_then(|n| n.as_u64())
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(TeleportConfig::default().max_open_per_dest),
                live_capable,
            })
            .await
            .map_err(Into::into)
    }
    pub(super) async fn capture(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        if row.kind == TeleportKind::Live {
            return capture_live(ctx, row).await;
        }
        let host = match ctx
            .state
            .host_registry
            .backend_for(row.source_host_id)
            .await
        {
            Ok(host) => host,
            Err(e) => return rollback_begin(ctx, row, e.to_string()).await,
        };
        let captured = host.snapshot_hold(row.source_sandbox_id, ctx.fence()).await;
        let metadata = match captured {
            Ok(m) => m,
            Err(e) => return rollback_begin(ctx, row, e.to_string()).await,
        };
        let record = SnapshotRecord {
            id: metadata.id,
            session_id: Some(row.session_id),
            host_id: Some(row.source_host_id),
            image_version: metadata.image_version.clone(),
            size_bytes: metadata.size_bytes,
            created_at: metadata.created_at,
            last_accessed_at: ctx.state.services.clock.now_utc(),
            disk_manifest: metadata.disk_manifest,
            memory_manifest: metadata.memory_manifest,
            recoverable: crate::api::snapshot::verify_snapshot_recoverable(
                ctx.state.services.blob.as_ref(),
                metadata.disk_manifest.as_ref(),
                metadata.memory_manifest.as_ref(),
            )
            .await,
            aux_bundles: metadata.aux_bundles,
            events_cursor: None,
            fc_snapshot_version: ctx
                .state
                .services
                .meta
                .fc_snapshot_version_for_host(row.source_host_id)
                .await?,
        };
        if !ctx
            .state
            .services
            .meta
            .fenced_record_snapshot(record, ctx.epoch)
            .await?
        {
            return Ok(Some(OpOutcome::Done));
        }
        advance(
            ctx,
            row,
            TeleportPhase::Captured,
            TeleportPatch {
                snapshot_id: Some(metadata.id),
                ..Default::default()
            },
        )
        .await
    }
    pub(super) async fn restore(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        if row.kind == TeleportKind::Live {
            let metadata = live_metadata(ctx, row).await?;
            let result = ctx
                .state
                .host_registry
                .backend_for(row.dest_host_id)
                .await?
                .restore(metadata, ctx.fence())
                .await;
            return finish_live_restore(ctx, row, result).await;
        }
        let state = ctx.state;
        let snapshot =
            state
                .services
                .meta
                .get_snapshot(row.snapshot_id.ok_or_else(|| {
                    ApiError::Internal("captured teleport has no snapshot".into())
                })?)
                .await?
                .ok_or_else(|| ApiError::Internal("teleport snapshot missing".into()))?;
        let session = state.services.meta.get_session(row.session_id).await?;
        let peer_hints = state
            .services
            .meta
            .get_host(row.source_host_id)
            .await?
            .and_then(|h| {
                crate::placement::host_can_serve_chunks(
                    &h,
                    state.services.clock.now_utc(),
                    crate::placement::placement_ttl(),
                )
                .map(str::to_owned)
            })
            .into_iter()
            .collect();
        let metadata = SnapshotMetadata {
            id: snapshot.id,
            size_bytes: snapshot.size_bytes,
            created_at: snapshot.created_at,
            image_version: snapshot.image_version,
            disk_manifest: snapshot.disk_manifest,
            memory_manifest: snapshot.memory_manifest,
            base_memory_manifest: crate::api::snapshot::base_memory_manifest_for_image(
                state,
                &session.image,
            )
            .await,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: Some(engram_chunk_store::snapshot_blob::state_blob_key(
                snapshot.id,
            )),
            sidecar_blob_key: Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(
                snapshot.id,
            )),
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: snapshot.aux_bundles,
            paused_at: None,
            peer_hints,
        };
        let host = match state.host_registry.backend_for(row.dest_host_id).await {
            Ok(host) => host,
            Err(e) => return rollback_begin(ctx, row, e.to_string()).await,
        };
        match host.restore(metadata, ctx.fence()).await {
            Ok(id) => {
                state
                    .host_registry
                    .record_sandbox_owner(id, row.dest_host_id);
                advance(
                    ctx,
                    row,
                    TeleportPhase::Restored,
                    TeleportPatch {
                        dest_sandbox_id: Some(id),
                        ..Default::default()
                    },
                )
                .await
            }
            Err(e) => rollback_begin(ctx, row, e.to_string()).await,
        }
    }
    pub(super) async fn commit(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        match ctx
            .state
            .services
            .meta
            .teleport_commit(row.id, ctx.epoch)
            .await?
        {
            Some(_) => {
                ctx.state
                    .host_registry
                    .invalidate_sandbox(row.source_sandbox_id);
                Ok(None)
            }
            None => rollback_begin(ctx, row, "commit_conflict".into()).await,
        }
    }
    pub(super) async fn attach(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        if destination_gone(ctx, row).await? {
            return fail_move(ctx, row, "dest_lost_after_blackout", true).await;
        }
        let state = ctx.state;
        let session = state.services.meta.get_session(row.session_id).await?;
        let sandbox = row.dest_sandbox_id.ok_or_else(|| {
            ApiError::Internal("committed teleport has no destination sandbox".into())
        })?;
        // A restart after the state transition still has to release the source.
        if matches!(session.status, SessionState::Active | SessionState::Created)
            && session.sandbox_id == Some(sandbox)
        {
            return advance(ctx, row, TeleportPhase::Attached, TeleportPatch::default()).await;
        }
        let (epoch, attached) = state
            .services
            .meta
            .session_binding_generations(row.session_id)
            .await?;
        crate::api::snapshot::bind_harness_generation(state, row.session_id, sandbox, epoch)
            .await?;
        let plan =
            crate::boot_materializer::materialize_snapshot_resume(state, &session, sandbox, epoch)
                .await?;
        attach_plan(ctx, row, plan, epoch, attached).await
    }
    pub(super) async fn attach_plan(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
        plan: crate::boot_materializer::HarnessPlan,
        epoch: u64,
        attached: u64,
    ) -> Result<Option<OpOutcome>, ApiError> {
        let state = ctx.state;
        let sandbox = row.dest_sandbox_id.expect("committed destination");
        if let crate::boot_materializer::HarnessPlan::Spawn { agent, policy } = plan {
            let host = state.host_registry.backend_for(row.dest_host_id).await?;
            match host.start_agent(sandbox, agent, *policy, ctx.fence()).await {
                Ok(()) => {}
                // The same classification ordinary resume uses: a guest
                // that cannot spawn this harness today will not spawn it
                // on retry, so the session rests at Created (/exec → 409).
                Err(e)
                    if matches!(&e, SandboxError::InvalidSpec(_))
                        || matches!(&e, SandboxError::HarnessSpawn { kind, .. }
                            if engram_core::harness_spawn_kind_is_deterministic(kind)) =>
                {
                    let error = e.to_string();
                    crate::session_ops::transition_with_fence_emitting(
                        state,
                        row.session_id,
                        ctx.fence(),
                        SessionState::Created,
                        BindingDisposition::Retain,
                        vec![SessionEvent::StatusChanged {
                            from: SessionState::Evacuating,
                            to: SessionState::Created,
                            at: state.services.clock.now_utc(),
                        }],
                    )
                    .await?;
                    return advance(
                        ctx,
                        row,
                        TeleportPhase::Attached,
                        TeleportPatch {
                            error: Some(error),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                Err(e) => return Err(e.into()),
            }
            if attached < epoch {
                return Ok(Some(OpOutcome::RetryAfter(
                    Duration::from_secs(2),
                    "waiting for harness generation".into(),
                )));
            }
        }
        let mut events = vec![SessionEvent::StatusChanged {
            from: SessionState::Evacuating,
            to: SessionState::Active,
            at: state.services.clock.now_utc(),
        }];
        if let Some(snapshot_id) = row.snapshot_id {
            events.insert(
                0,
                SessionEvent::Resumed {
                    snapshot_id,
                    at: state.services.clock.now_utc(),
                },
            );
        }
        crate::session_ops::transition_with_fence_emitting(
            state,
            row.session_id,
            ctx.fence(),
            SessionState::Active,
            BindingDisposition::Retain,
            events,
        )
        .await?;
        advance(ctx, row, TeleportPhase::Attached, TeleportPatch::default()).await
    }
    pub(super) async fn release(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        if destination_gone(ctx, row).await? {
            return fail_move(ctx, row, "dest_lost_after_blackout", true).await;
        }
        if row.kind == TeleportKind::Live {
            let dest = ctx
                .state
                .host_registry
                .backend_for(row.dest_host_id)
                .await?;
            match dest
                .migration_drain_wait(row.dest_sandbox_id.expect("attached destination"))
                .await?
            {
                engram_core::types::snapshot::DrainOutcome::Done { .. } => {}
                engram_core::types::snapshot::DrainOutcome::PeerLost { .. } => {
                    return fail_move(ctx, row, "peer_lost", true).await
                }
            }
            if !source_gone(ctx, row).await? {
                match ctx
                    .state
                    .host_registry
                    .backend_for(row.source_host_id)
                    .await?
                    .migration_commit(
                        row.source_sandbox_id,
                        row.export_id.as_deref().expect("live export"),
                        ctx.fence(),
                    )
                    .await
                {
                    // Already consumed: an earlier commit passed its point
                    // of no return, or the host's TTL sweep took it. The
                    // destroy below confirms the source is gone either way.
                    Ok(()) | Err(SandboxError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let result = match ctx
            .state
            .host_registry
            .backend_for(row.source_host_id)
            .await
        {
            Ok(host) => host.destroy(row.source_sandbox_id, ctx.fence()).await,
            Err(e) => Err(e),
        };
        let how = match result {
            Ok(()) | Err(SandboxError::NotFound) => SourceRelease::DestroyAcked,
            Err(_) if source_gone(ctx, row).await? => SourceRelease::SourceHostGone,
            Err(e) => {
                return Ok(Some(OpOutcome::RetryAfter(
                    Duration::from_secs(10),
                    e.to_string(),
                )))
            }
        };
        ctx.state
            .services
            .meta
            .teleport_release_source(row.id, ctx.epoch, how)
            .await?;
        Ok(Some(OpOutcome::Done))
    }
    /// The source is gone during a rollback (dead or retired host, or a
    /// destroyed sandbox): entomb it and settle the session the way a lost
    /// host settles it (`dead_host::recovery_target`): Idle with a
    /// recoverable memory snapshot or a live disk manifest (the disk-only
    /// cold boot), Dead with nothing recoverable.
    async fn settle_lost_source(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        let meta = &ctx.state.services.meta;
        let session = meta.get_session(row.session_id).await?;
        let target = settle_target(ctx, row, &session, true).await?;
        meta.teleport_settle(
            row.id,
            ctx.epoch,
            TeleportSettle {
                error: "source_lost_during_rollback".into(),
                session: Some(target),
                entomb_source: true,
            },
        )
        .await?;
        Ok(Some(OpOutcome::Done))
    }
    pub(super) async fn rollback(
        ctx: &OpCtx<'_>,
        row: &TeleportRow,
    ) -> Result<Option<OpOutcome>, ApiError> {
        destroy_destination(ctx, row).await?;
        if source_gone(ctx, row).await? {
            return settle_lost_source(ctx, row).await;
        }
        let host = ctx
            .state
            .host_registry
            .backend_for(row.source_host_id)
            .await?;
        // A live move that reached presetup fenced its export on the source;
        // migration_abort unfences it and restores the presetup state. A
        // snapshot move (or a live move downgraded before presetup) was held
        // paused by snapshot_hold; resume wakes it and re-arms swap.
        let resumed = match row.export_id.as_deref() {
            Some(export_id) if row.kind == TeleportKind::Live => {
                match host
                    .migration_abort(row.source_sandbox_id, export_id, ctx.fence())
                    .await
                {
                    Ok(()) => Ok(()),
                    // The export is already consumed: an earlier abort passed
                    // its point of no return, or the host's TTL sweep took it.
                    // Only the un-pause can still be missing, and resume is
                    // idempotent on a running guest. Active is declared only
                    // on a resume ack, never on the consumed export alone.
                    Err(SandboxError::NotFound) => {
                        host.resume(row.source_sandbox_id, ctx.fence()).await
                    }
                    Err(e) => Err(e),
                }
            }
            _ => host.resume(row.source_sandbox_id, ctx.fence()).await,
        };
        match resumed {
            Ok(()) => {}
            // The source sandbox itself is gone: there is nothing to go back
            // to, so the move fails honestly instead of retrying forever.
            Err(SandboxError::NotFound) => return settle_lost_source(ctx, row).await,
            Err(e) => {
                return Ok(Some(OpOutcome::RetryAfter(
                    Duration::from_secs(5),
                    e.to_string(),
                )))
            }
        }
        let session = ctx.state.services.meta.get_session(row.session_id).await?;
        if session.status == SessionState::Evacuating {
            crate::session_ops::transition_with_fence_emitting(
                ctx.state,
                row.session_id,
                ctx.fence(),
                SessionState::Active,
                BindingDisposition::Retain,
                vec![SessionEvent::StatusChanged {
                    from: SessionState::Evacuating,
                    to: SessionState::Active,
                    at: ctx.state.services.clock.now_utc(),
                }],
            )
            .await?;
        }
        ctx.state
            .services
            .meta
            .teleport_abort(row.id, ctx.epoch)
            .await?;
        Ok(Some(OpOutcome::Done))
    }
}

#[cfg(test)]
use crate as coordinator;
#[cfg(test)]
#[path = "../tests/support/teleport_scenarios.rs"]
mod tests;

#[cfg(test)]
mod attach_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tests::support::Rig;
    async fn committed() -> Rig {
        let rig = Rig::sim().await;
        let meta = &rig.state.services.meta;
        let epoch = rig.op.epoch.unwrap();
        meta.teleport_advance(
            rig.row.id,
            TeleportPhase::Admitted,
            TeleportPhase::Captured,
            TeleportPatch::default(),
            epoch,
        )
        .await
        .unwrap();
        meta.teleport_advance(
            rig.row.id,
            TeleportPhase::Captured,
            TeleportPhase::Restored,
            TeleportPatch {
                dest_sandbox_id: Some(rig.dest.sandbox),
                ..Default::default()
            },
            epoch,
        )
        .await
        .unwrap();
        meta.teleport_commit(rig.row.id, epoch)
            .await
            .unwrap()
            .unwrap();
        rig.state
            .host_registry
            .record_sandbox_owner(rig.dest.sandbox, rig.row.dest_host_id);
        rig
    }
    fn plan(rig: &Rig, epoch: u64) -> crate::boot_materializer::HarnessPlan {
        crate::boot_materializer::HarnessPlan::Spawn {
            agent: engram_core::types::sandbox::AgentSpec {
                argv: vec!["harness".into()],
                env: Default::default(),
                session_env: Default::default(),
                binding_epoch: epoch,
                host_ca_pem: None,
            },
            policy: Box::new(crate::api::snapshot::placeholder_egress_policy(
                rig.row.session_id,
                rig.dest.sandbox,
            )),
        }
    }
    #[tokio::test]
    async fn attach_waits_for_the_new_generation_before_active() {
        let rig = committed().await;
        let meta = &rig.state.services.meta;
        let row = meta
            .open_teleport_for_session(rig.row.session_id)
            .await
            .unwrap()
            .unwrap();
        let (generation, attached) = meta
            .session_binding_generations(row.session_id)
            .await
            .unwrap();
        let ctx = OpCtx {
            state: &rig.state,
            op: &rig.op,
            epoch: rig.op.epoch.unwrap(),
        };
        assert!(matches!(
            steps::attach_plan(&ctx, &row, plan(&rig, generation), generation, attached)
                .await
                .unwrap(),
            Some(OpOutcome::RetryAfter(_, _))
        ));
        assert_eq!(
            meta.get_session(row.session_id).await.unwrap().status,
            SessionState::Evacuating
        );
        meta.settle_harness_generation(
            row.session_id,
            generation,
            &[],
            rig.state.services.clock.now_utc(),
        )
        .await
        .unwrap();
        let (_, attached) = meta
            .session_binding_generations(row.session_id)
            .await
            .unwrap();
        assert!(
            steps::attach_plan(&ctx, &row, plan(&rig, generation), generation, attached)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            meta.get_session(row.session_id).await.unwrap().status,
            SessionState::Active
        );
    }
    #[tokio::test]
    async fn attach_deterministic_failure_rests_at_created_and_still_releases_source() {
        let rig = committed().await;
        rig.dest.spawn_fails.store(true, Ordering::SeqCst);
        let meta = &rig.state.services.meta;
        let row = meta
            .open_teleport_for_session(rig.row.session_id)
            .await
            .unwrap()
            .unwrap();
        let (generation, attached) = meta
            .session_binding_generations(row.session_id)
            .await
            .unwrap();
        let ctx = OpCtx {
            state: &rig.state,
            op: &rig.op,
            epoch: rig.op.epoch.unwrap(),
        };
        assert!(
            steps::attach_plan(&ctx, &row, plan(&rig, generation), generation, attached)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            meta.get_session(row.session_id).await.unwrap().status,
            SessionState::Created
        );
        assert!(matches!(rig.drive().await, OpOutcome::Done));
        assert_eq!(rig.source.destroys.load(Ordering::SeqCst), 1);
    }
}
