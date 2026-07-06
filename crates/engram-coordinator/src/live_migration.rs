//! ADR 0045 C2: the POST-COPY live-teleport coordinator verb (the
//! clean-break replacement of C1's stop-and-copy; snapshot-rehome is
//! the only fallback).
//!
//! `migrate_session_live` under the session lease + the R8 host gate:
//! `migration_presetup` on the source (pre-pause: export identity +
//! the dest's restore package) → SPAWN the dest restore as a task (it
//! pre-stages netns/FC/handler concurrently with everything below; its
//! handler parks at the source's page server until the SEAL, and its
//! FC load gates on `state.bin` landing) → mark `Evacuating` (the
//! crash parachute) → `migration_capture_postcopy` (THE BLACKOUT:
//! pause → NBD drain → fork-v3 vmstate-only → pagemap seal) → await
//! the restore task → the `Committing` persist (one atomic
//! `rebind_session` UPDATE — the ownership oracle flips with it) →
//! emit `evacuating → active` (post-blackout; the prompt-hold keeps
//! "messages deliver" honest through the harness rebuild) → finalize
//! task: `migration_drain_wait` (the dest pulls every sealed chunk) →
//! commit (destroy) the source. Durability is NOT migration's job
//! (operator decision, 2026-06-12, superseding the opening bookend's
//! D-decision): the dest simply joins the periodic checkpoint cadence
//! like any resumed session — its first periodic capture is a safe
//! Full (no chain) — and until that lands, recovery rewinds to the
//! SOURCE's last periodic row (RPO ≤ cadence, the accepted model).
//! The old finalize took an immediate post-move Full here; that was a
//! guest-visible 2-4s pause seconds after landing, paid on every
//! single move to shave the rewind window — backwards, given the
//! goal is a teleport humans can't notice.
//!
//! Failure arms: presetup/dest-prep failures before the pause ⇒
//! `Unsupported`/`Fatal`, session untouched (snapshot-rehome fallback).
//! Capture failure ⇒ best-effort resume-in-place + walk back to Active
//! (zero loss). Dest restore failing with the `postcopy-never-loaded`
//! marker (its FC load gate timed out — the dest PROVABLY never ran
//! the shipped state) ⇒ `migration_abort` un-pauses the source (zero
//! loss); any other/ambiguous restore failure ⇒ parachute (scanner
//! rehome from the durable row, or kill when none exists). PeerLost
//! mid-drain ⇒ the finalize destroys the poisoned dest and re-arms
//! `Evacuating` (rung-1 rewind).

use engram_core::types::snapshot::MigrationSourceInfo;
use engram_core::types::SessionState;
use engram_core::SandboxError;
use engram_core::{HostId, SessionId};
use tracing::Instrument;

use crate::idle_evictor::SessionLeaseGuard;
use crate::state::{SessionEvent, SharedState};

#[derive(Debug)]
pub enum MigrateError {
    /// Source or destination can't do a live move — fall back to
    /// snapshot-rehome (the pre-C1 teleport).
    Unsupported(String),
    /// The move failed but the source was aborted back to Active —
    /// downtime only, zero loss.
    AbortedToSource(String),
    /// The move failed in a state the parachute owns: the session is
    /// `Evacuating` and the scanner will rehome from the last durable
    /// checkpoint (loss ≤ one cadence).
    Parachute(String),
    Fatal(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(m) => write!(f, "live migration unsupported: {m}"),
            Self::AbortedToSource(m) => write!(f, "live migration aborted to source: {m}"),
            Self::Parachute(m) => write!(f, "live migration failed; scanner rehome armed: {m}"),
            Self::Fatal(m) => write!(f, "live migration failed: {m}"),
        }
    }
}

/// Feature gate: `ENGRAM_LIVE_TELEPORT=1`. Off ⇒ the teleport verb keeps
/// the snapshot-rehome path unconditionally.
pub fn live_teleport_enabled() -> bool {
    std::env::var("ENGRAM_LIVE_TELEPORT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// ADR 0045 C2 (R8): at most ONE in-flight migration per host endpoint
/// — a consolidation wave would otherwise double P2P+GCS pressure on a
/// single source/dest. Process-local (coordinator pods are effectively
/// singular today; the session lease already serializes per-session).
static MIGRATION_GATE: std::sync::LazyLock<dashmap::DashMap<HostId, SessionId>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);

/// Both-endpoint claim; `Drop` releases both. Holds source AND dest for
/// the whole move INCLUDING the finalize drain (the guard rides into
/// the finalize task).
struct MigrationGateGuard {
    hosts: Vec<HostId>,
}

impl MigrationGateGuard {
    fn claim(source: Option<HostId>, dest: HostId, session: SessionId) -> Option<Self> {
        let mut claimed = Vec::new();
        for host in source.into_iter().chain([dest]) {
            // Compute the claim outcome and DROP the entry guard before
            // any rollback `remove` — holding a shard lock across a
            // `remove` of a host that hashes to the SAME shard
            // self-deadlocks (DashMap shards are RwLocks; fewer shards
            // on low-core CI made the collision a real hang).
            let inserted = match MIGRATION_GATE.entry(host) {
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(session);
                    true
                }
                dashmap::mapref::entry::Entry::Occupied(_) => false,
            };
            if inserted {
                claimed.push(host);
            } else {
                // Roll back partial claims — no entry guard held now.
                for h in claimed {
                    MIGRATION_GATE.remove(&h);
                }
                return None;
            }
        }
        Some(Self { hosts: claimed })
    }
}

impl Drop for MigrationGateGuard {
    fn drop(&mut self) {
        for h in &self.hosts {
            MIGRATION_GATE.remove(h);
        }
    }
}

// ADR 0019 / telemetry restoration (#526): this verb is driven both from
// an admin request fan-out and (indirectly) from scanner-driven drain —
// neither reliably supplies a request span. An explicit root (carrying
// `session_id`/`target_host_id`) means the pipeline's own detached spawns
// below (dest restore, drain+commit finalize) have something real to
// `.instrument(Span::current())` onto instead of orphaning.
#[tracing::instrument(name = "live_migration.migrate_session_live", skip_all, fields(%session_id, %target_host_id))]
pub async fn migrate_session_live(
    state: &SharedState,
    session_id: SessionId,
    target_host_id: HostId,
) -> Result<(), MigrateError> {
    let t_total = std::time::Instant::now();
    // Serialize against resumes / evictions / sibling migrations.
    let session = state
        .services
        .meta
        .get_session(session_id)
        .await
        .map_err(|e| MigrateError::Fatal(format!("get_session: {e}")))?;
    if session.status != SessionState::Active {
        return Err(MigrateError::Fatal(format!(
            "live migration needs an Active session (got {})",
            session.status.as_str(),
        )));
    }
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(MigrateError::Fatal("no bound sandbox".into()));
    };
    let lease = match SessionLeaseGuard::try_acquire(state, session_id, Some(sandbox_id)).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return Err(MigrateError::Fatal(
                "session is mid-resume/eviction/migration (lease held)".into(),
            ))
        }
        Err(e) => return Err(MigrateError::Fatal(format!("lease acquire: {e}"))),
    };

    // The destination must be takeable and the source addressable
    // before we freeze anything.
    let (_, dest_backend) = crate::placement::pick_specific_host(
        state.services.meta.as_ref(),
        &state.host_registry,
        target_host_id,
        session.host_id,
    )
    .await
    .map_err(|e| MigrateError::Fatal(format!("target host can't take the session: {e:?}")))?;
    let source_addr = source_host_addr(state, session.host_id)
        .await
        .ok_or_else(|| {
            MigrateError::Unsupported("source host has no advertised host_addr".into())
        })?;

    // The last durable checkpoint row: the metadata template AND the
    // parachute's landing spot. OPTIONAL by operator decision
    // (2026-06-11): a session with no durable row yet (younger than
    // its first periodic checkpoint) teleports anyway — if the move
    // fails mid-flight there is nothing to rehome from and the
    // session is lost. Metadata falls back to the image's base
    // snapshot row (aux_bundles must still pin, or a [git] image
    // restores without its skills mounts).
    let durable_row = state
        .services
        .meta
        .latest_snapshot_for_session(session_id)
        .await
        .map_err(|e| MigrateError::Fatal(format!("latest snapshot: {e}")))?;
    let base_row = match &durable_row {
        Some(_) => None,
        None => match state
            .services
            .meta
            .get_enabled_image_any(&session.image)
            .await
        {
            Ok(Some(img)) => match img.base_snapshot_id {
                Some(id) => state.services.meta.get_snapshot(id).await.ok().flatten(),
                None => None,
            },
            _ => None,
        },
    };
    if durable_row.is_none() {
        tracing::warn!(
            %session_id,
            "live migration without a durable checkpoint row — a mid-move failure past the freeze CANNOT be rehomed (operator-accepted)",
        );
    }

    // Resolve the source's backend handle ONCE, pre-freeze, and use it
    // for capture, the failure-arm aborts, AND the post-rebind commit.
    // Commit cannot route by sandbox id: step 4's
    // `invalidate_sandbox(sandbox_id)` drops the old route on purpose
    // (the old sandbox must stop serving exec), and the PG read-through
    // finds nothing because the session row already points at the new
    // sandbox — prod canary 5fa742b7 hit exactly this ("sandbox not
    // found" on commit; the source stayed frozen until the export TTL
    // destroyed it ~2 min later).
    let source_backend = match state.host_registry.resolve_owner(sandbox_id).await {
        Ok((_, backend)) => backend,
        Err(e) => return Err(MigrateError::Fatal(format!("resolve source host: {e}"))),
    };

    // ---- R8 gate: one in-flight migration per host endpoint ----
    let Some(gate_guard) = MigrationGateGuard::claim(session.host_id, target_host_id, session_id)
    else {
        return Err(MigrateError::Fatal(
            "another migration is in flight on the source or destination host (R8)".into(),
        ));
    };

    // ---- 1. Presetup on the source (NO pause — the guest runs) ----
    let t_presetup = std::time::Instant::now();
    let presetup = match source_backend.migration_presetup(sandbox_id).await {
        Ok(p) => p,
        Err(SandboxError::InvalidSpec(reason)) => {
            return Err(MigrateError::Unsupported(reason));
        }
        Err(e) => return Err(MigrateError::Fatal(format!("migration presetup: {e}"))),
    };
    let presetup_ms = t_presetup.elapsed().as_millis();

    // The page-server address: the source's advertised gRPC host with
    // the presetup's peer port (default 9102).
    let peer_addr = {
        let hostpart = source_addr
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let host = hostpart
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(hostpart);
        format!("{host}:{}", presetup.peer_port)
    };

    // ---- 2. Spawn the destination restore (pre-stage) ----
    // Runs CONCURRENTLY with the blackout below: netns + FC spawn +
    // handler bring-up happen while the guest still runs; the handler
    // parks at the page server for the SEAL and the FC load gates on
    // `state.bin` (landed by the dest's fetch poller once the capture
    // opens the export). No artifacts exist yet by design.
    let base_memory_manifest =
        crate::api::snapshot::base_memory_manifest_for_image(state, &session.image).await;
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        // The restore stages under a FRESH id (post-copy mints no
        // capture-time snapshot id the dest could collide on).
        id: engram_core::types::SnapshotId::new(),
        size_bytes: durable_row.as_ref().map(|r| r.size_bytes).unwrap_or(0),
        created_at: chrono::Utc::now(),
        image_version: durable_row
            .as_ref()
            .map(|r| r.image_version.clone())
            .or_else(|| base_row.as_ref().map(|r| r.image_version.clone()))
            .unwrap_or_else(|| {
                session
                    .image
                    .rsplit(':')
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            }),
        base_memory_manifest,
        migration_source: Some(MigrationSourceInfo {
            export_id: presetup.export_id.clone(),
            source_addr: source_addr.clone(),
            memory_manifest_json: presetup.memory_manifest_json.clone(),
            disk_manifest_json: Vec::new(),
            memory_manifest_ref: presetup.memory_manifest_ref,
            disk_manifest_ref: presetup
                .disk_manifest_ref
                .unwrap_or_else(engram_core::types::manifest::ManifestRef::new),
            new_memory_chunk_hashes: Vec::new(),
            new_disk_chunk_hashes: Vec::new(),
            hot_chunks: presetup.hot_chunks.clone(),
            post_copy: true,
            peer_addr: Some(peer_addr),
            peer_token: Some(presetup.peer_token.clone()),
            sidecar_json: presetup.sidecar_json.clone(),
        }),
        disk_manifest: presetup.disk_manifest_ref,
        memory_manifest: Some(presetup.memory_manifest_ref),
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: durable_row
            .as_ref()
            .map(|r| r.aux_bundles.clone())
            .or_else(|| base_row.as_ref().map(|r| r.aux_bundles.clone()))
            .unwrap_or_default(),
        // Issue #529: restore-side reconstruction, not a fresh capture —
        // no pause instant to carry.
        paused_at: None,
    };
    // ADR 0019 / telemetry restoration (#526): `dest.restore` makes a
    // gRPC call to the target host-agent; the `TraceparentInjector`
    // interceptor propagates whatever span is current at call time onto
    // the wire. Detaching via bare `tokio::spawn` would send an empty
    // traceparent and orphan the host-side restore spans from this
    // migration trace.
    let restore_task = {
        let dest = dest_backend.clone();
        let restore_span = tracing::Span::current();
        tokio::spawn(async move { dest.restore(metadata).await }.instrument(restore_span))
    };

    // ---- 3. Arm the parachute ----
    // From here until the rebind, a coordinator death leaves the
    // session Evacuating: lease expiry → scanner snapshot-rehome from
    // `durable_row`. The frozen source self-cleans via the export TTL
    // (whose yes⇒un-pause arm is FORBIDDEN once state.bin shipped).
    if let Err(e) = state
        .services
        .meta
        .transition_session(session_id, SessionState::Evacuating)
        .await
    {
        restore_task.abort();
        return Err(MigrateError::Fatal(format!("transition: {e}")));
    }
    if let Err(e) = state
        .services
        .meta
        .set_teleport_target(session_id, Some(target_host_id))
        .await
    {
        tracing::warn!(%session_id, error = %e,
            "set_teleport_target failed; parachute would fall back to any-peer");
    }
    // Observer-facing truth: the session leaves `active` as the
    // blackout begins.
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Active,
                to: SessionState::Evacuating,
                at: chrono::Utc::now(),
            },
        )
        .await;

    // ---- 4. THE BLACKOUT: vmstate-only capture + pagemap seal ----
    let t_blackout = std::time::Instant::now();
    let capture = match source_backend
        .migration_capture_postcopy(sandbox_id, &presetup.export_id)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            // Nothing shipped; the guest may or may not be paused
            // depending on where capture failed — resume is the
            // conservative un-freeze (idempotent enough: resuming a
            // running VM is a benign FC error).
            restore_task.abort();
            let _ = source_backend.resume(sandbox_id).await;
            let _ = state
                .services
                .meta
                .set_teleport_target(session_id, None)
                .await;
            if walk_back_to_active(state, session_id).await {
                return Err(MigrateError::AbortedToSource(format!(
                    "post-copy capture: {e}"
                )));
            }
            return Err(parachute_or_kill(
                state,
                session_id,
                durable_row.is_some(),
                format!("post-copy capture: {e}"),
            )
            .await);
        }
    };
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "capture_postcopy")
        .record(t_blackout.elapsed().as_secs_f64());

    // ---- 5. Await the destination (load + resume) ----
    let t_restore = std::time::Instant::now();
    let new_sandbox_id = match restore_task.await {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            let _ = state
                .services
                .meta
                .set_teleport_target(session_id, None)
                .await;
            // The load-gate marker proves the dest never ran the
            // shipped state — un-pausing the source is zero-loss
            // sound. Anything else is ambiguous: parachute.
            if e.to_string().contains("postcopy-never-loaded") {
                let abort_ok = source_backend
                    .migration_abort(sandbox_id, &presetup.export_id)
                    .await
                    .is_ok();
                if abort_ok && walk_back_to_active(state, session_id).await {
                    metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => "aborted_to_source")
                        .increment(1);
                    return Err(MigrateError::AbortedToSource(format!("dest restore: {e}")));
                }
            }
            return Err(parachute_or_kill(
                state,
                session_id,
                durable_row.is_some(),
                format!("dest restore: {e}"),
            )
            .await);
        }
        Err(join_err) => {
            let _ = state
                .services
                .meta
                .set_teleport_target(session_id, None)
                .await;
            return Err(parachute_or_kill(
                state,
                session_id,
                durable_row.is_some(),
                format!("dest restore task: {join_err}"),
            )
            .await);
        }
    };
    let restore_ms = t_restore.elapsed().as_millis();
    // The guest is RUNNING on the dest from here (FC resumed inside
    // the load) — this is where the guest-observed blackout ends. The
    // rebind + harness rebuild below happen while the guest executes,
    // so folding them into `blackout_ms` (the old shape) overstated
    // the user-facing gap by the `finish_resume_to_active` wall.
    let blackout_wall_ms = t_blackout.elapsed().as_millis() as u64;
    let t_reactivate = std::time::Instant::now();

    // ---- 6. The Committing persist + reactivate ----
    state.host_registry.invalidate_sandbox(sandbox_id);
    state
        .host_registry
        .record_sandbox_owner(new_sandbox_id, target_host_id);
    let rebind = async {
        // ONE atomic UPDATE: the ownership oracle (`sandbox_ownership`)
        // flips with it — the post-copy ownership transfer point. The
        // source's TTL answer goes `false` from here.
        //
        // Issue #211: guard the rebind on the row still being the
        // `Evacuating` row bound to the SOURCE sandbox we're migrating
        // off. A `DELETE /sessions/:id` (or a reconcile strike) racing
        // the copy can flip the row terminal and/or clear its sandbox;
        // a blind rebind would re-bind the new live VM onto that terminal
        // row, the ownership oracle would answer `owned = true`, and the
        // orphan reap would never fire. On `Conflict` we drop into the
        // error arm below, which destroys `new_sandbox_id` and parachutes.
        state
            .services
            .meta
            .rebind_session_guarded(
                session_id,
                target_host_id,
                new_sandbox_id,
                Some(Some(sandbox_id)),
                &[SessionState::Evacuating],
            )
            .await
            .map_err(|e| format!("rebind: {e}"))?;
        state
            .services
            .meta
            .transition_session(session_id, SessionState::Created)
            .await
            .map_err(|e| format!("to Created: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(e) = rebind {
        let _ = dest_backend.destroy(new_sandbox_id).await;
        let _ = state
            .services
            .meta
            .set_teleport_target(session_id, None)
            .await;
        return Err(parachute_or_kill(state, session_id, durable_row.is_some(), e).await);
    }
    // ADR 0073: live move — the harness process SURVIVES the teleport
    // (agentd C1 reattach), so its generation is unchanged: bind the
    // target host's record at the CURRENT epoch (re-point, no mint).
    let epoch = state
        .services
        .meta
        .current_binding_epoch(session_id)
        .await
        .unwrap_or(0);
    crate::api::snapshot::bind_session_routing(state, session_id, new_sandbox_id, epoch).await;
    // CASE 1 (issue #209): the teleport_target pin's ONLY job is to aim
    // the parachute at the dest while the move is in flight. The rebind
    // above committed the ownership flip — the dest is now the durable
    // owner — so the pin has done its job and MUST be cleared here,
    // BEFORE the fallible refresh / finish_resume_to_active below. The
    // pin is PG-durable (read back at evac_resumer's get_teleport_target,
    // honored strictly as require_host), so leaking it on an early `?`
    // return pins every future rehome of this session to this host for
    // the process lifetime. Clearing it unconditionally on this exit
    // path closes that leak; if the move rewinds further down, the
    // parachute re-arms its own targeting.
    let _ = state
        .services
        .meta
        .set_teleport_target(session_id, None)
        .await;
    // D12: `evacuating → active` emits POST-BLACKOUT (the guest is
    // executing on the dest). The prompt-hold (session lease) keeps
    // "messages deliver" honest through the harness rebuild below.
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Evacuating,
                to: SessionState::Active,
                at: chrono::Utc::now(),
            },
        )
        .await;
    // A failed refresh / finish_resume_to_active here must NOT abandon
    // the move with the source still alive: the session is already
    // rebound + Active on the dest, the finalize (drain + source commit)
    // still has to run. Fall back to a best-effort log instead of an
    // early `?` return (which historically skipped both the pin removal
    // above AND the finalize spawn below — the pre-spawn gap). The
    // harness rebuild is recoverable post-hoc; abandoning the source
    // page server is not.
    match state.services.meta.get_session(session_id).await {
        Ok(session_refreshed) => {
            if let Err(e) = crate::api::snapshot::finish_resume_to_active(
                state,
                &session_refreshed,
                new_sandbox_id,
                false,
            )
            .await
            {
                tracing::warn!(%session_id, error = %e,
                    "post-copy migration: finish_resume_to_active failed; session left at Created");
            }
        }
        Err(e) => {
            tracing::warn!(%session_id, error = %e,
                "post-copy migration: session refresh failed; skipping harness rebuild \
                 but proceeding to the finalize (drain + source release)");
        }
    }

    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "presetup")
        .record(presetup_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "restore")
        .record(restore_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "total")
        .record(t_total.elapsed().as_secs_f64());
    // PR 10 blackout decomposition: the legs that actually cost, so an
    // optimization targets the real hot leg. Source-measured (under the
    // freeze); the coordinator-side `blackout_ms` is the wall including
    // the round trip.
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "blackout_pause")
        .record(capture.pause_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "blackout_disk_drain")
        .record(capture.disk_drain_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "blackout_vmstate")
        .record(capture.vmstate_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "blackout_scan")
        .record(capture.scan_ms as f64 / 1000.0);
    metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => "migrated_postcopy")
        .increment(1);
    tracing::info!(
        %session_id,
        old_sandbox = %sandbox_id,
        new_sandbox = %new_sandbox_id,
        target_host = %target_host_id,
        presetup_ms,
        sealed_chunks = capture.sealed_chunks,
        total_chunks = capture.total_chunks,
        sealed_disk_chunks = capture.sealed_disk_chunks,
        // Blackout decomposition (source-measured, under the freeze):
        // pause + disk_drain + vmstate + scan ≈ the host-side blackout;
        // `blackout_ms` is the coordinator wall from capture start to
        // the dest restore returning (guest running) — the closest
        // coordinator-side proxy for the guest-observed gap.
        // `reactivate_ms` (rebind + emits + harness rebuild) runs
        // while the guest already executes.
        blackout_pause_ms = capture.pause_ms,
        blackout_disk_drain_ms = capture.disk_drain_ms,
        blackout_vmstate_ms = capture.vmstate_ms,
        scan_ms = capture.scan_ms,
        blackout_ms = blackout_wall_ms,
        restore_await_ms = restore_ms,
        reactivate_ms = t_reactivate.elapsed().as_millis() as u64,
        total_ms = t_total.elapsed().as_millis(),
        "post-copy live teleport landed (ADR 0045 C2); drain + durability finalizing",
    );

    // ---- 7. Finalize: drain → commit source → Full checkpoint → row ----
    // The lease + the R8 gate ride into the task. The dest keeps
    // serving the user throughout; the SOURCE stays alive as a page
    // server until DrainDone.
    //
    // ADR 0019 / telemetry restoration (#526): re-parent onto the
    // migration span so the finalize (drain + source commit) stitches
    // under the same trace instead of exporting as an orphaned root.
    let state2 = state.clone();
    let export_id = presetup.export_id.clone();
    let finalize_span = tracing::Span::current();
    tokio::spawn(
        async move {
        let lease = lease;
        let _gate_guard = gate_guard;

        // 7a. The drain: every sealed chunk lands on the dest. Wrapped in
        // a bounded-backoff retry (CASE 2, issue #209): a TRANSIENT
        // transport Err on `migration_drain_wait` (a dest host-agent pod
        // roll, a network blip) used to hit a bare `return` that left the
        // session Active on the dest while the source's export TTL fired
        // — the ownership probe then answers owned=false and the host
        // DESTROYS the frozen source's page server, which the dest may
        // still need, poisoning it. Retry the RPC a few times; only on
        // PERSISTENT failure take the SAME rung-1 rewind as PeerLost
        // (destroy dest, re-arm Evacuating, parachute_or_kill) — never
        // leave an Active session whose page server gets reaped.
        const DRAIN_RETRY_BUDGET: u32 = 5;
        // Base backoff (doubled each attempt: 2s, 4s, 8s, …). Overridable
        // via `ENGRAM_MIGRATION_DRAIN_RETRY_BASE_MS` for ops tuning and so
        // the regression test can drive the exhaustion path without a
        // ~30s real wait.
        let drain_retry_base = std::time::Duration::from_millis(
            std::env::var("ENGRAM_MIGRATION_DRAIN_RETRY_BASE_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(2000),
        );
        let t_drain = std::time::Instant::now();
        let mut last_err: Option<engram_core::SandboxError> = None;
        let drain_outcome = 'retry: {
            for attempt in 0..DRAIN_RETRY_BUDGET {
                // Refresh the lease across attempts the same way the wait
                // below does, so a multi-attempt retry doesn't outlive the
                // lease and stomp a session another holder re-acquired.
                let mut touch = tokio::time::interval(std::time::Duration::from_secs(60));
                touch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                touch.tick().await;

                let drain = state2.services.host.migration_drain_wait(new_sandbox_id);
                tokio::pin!(drain);
                let res = loop {
                    tokio::select! {
                        res = &mut drain => break res,
                        _ = touch.tick() => {
                            match lease.touch_checked().await {
                                crate::idle_evictor::LeaseTouch::Held => {}
                                // A transport blip on the touch is NOT loss
                                // (CASE: lease touch). Keep waiting; the
                                // 180s reaper still backstops a truly dead
                                // holder, and the drain RPC remains in flight.
                                crate::idle_evictor::LeaseTouch::TransientError(e) => {
                                    tracing::warn!(%session_id, error = %e,
                                        "post-copy finalize: lease touch transport error — retrying, not abandoning");
                                }
                                crate::idle_evictor::LeaseTouch::Lost => {
                                    tracing::error!(%session_id,
                                        "post-copy finalize: lease lost mid-drain (reaped/re-acquired)");
                                    return;
                                }
                            }
                        }
                    }
                };
                match res {
                    Ok(outcome) => break 'retry Ok(outcome),
                    Err(e) => {
                        tracing::warn!(%session_id, error = %e, attempt = attempt + 1,
                            budget = DRAIN_RETRY_BUDGET,
                            "post-copy drain_wait transport error; retrying with backoff");
                        last_err = Some(e);
                        // Exponential backoff (2s, 4s, 8s, …) — bounded by
                        // the budget so the whole retry stays well inside
                        // the source export TTL when started promptly.
                        if attempt + 1 < DRAIN_RETRY_BUDGET {
                            tokio::time::sleep(drain_retry_base * (1 << attempt)).await;
                        }
                    }
                }
            }
            // Budget exhausted: fall through to the rewind with the last
            // transport error as the detail.
            break 'retry Err(last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "drain_wait exhausted retries".into()));
        };

        // Normalize: a PeerLost outcome and a persistent transport Err
        // both demand the rung-1 rewind. Map both into a single rewind
        // detail so they share one arm (the page server is doomed either
        // way once we stop owning the drain).
        let rewind_detail: Option<String> = match drain_outcome {
            Ok(engram_core::types::snapshot::DrainOutcome::Done {
                pulled,
                alt_sourced,
                zero_chunks,
                ms,
            }) => {
                metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "drain")
                    .record(t_drain.elapsed().as_secs_f64());
                tracing::info!(
                    %session_id, pulled, alt_sourced, zero_chunks, ms,
                    "post-copy drain complete; releasing the source",
                );
                None
            }
            Ok(engram_core::types::snapshot::DrainOutcome::PeerLost { remaining, detail }) => {
                tracing::error!(%session_id, remaining, %detail,
                    "post-copy drain lost its peer — rewinding the whole VM (rung-1)");
                Some(format!("peer lost mid-drain: {detail}"))
            }
            Err(detail) => {
                tracing::error!(%session_id, %detail,
                    "post-copy drain_wait failed after retries — rewinding the whole VM (rung-1)");
                Some(format!("drain transport error after retries: {detail}"))
            }
        };
        if let Some(detail) = rewind_detail {
            // Rung-1 rewind: the dest holds unfillable state (or we can no
            // longer drive the drain). Destroy it, re-arm Evacuating, and
            // let the scanner rehome from the durable row (or kill when
            // none exists). The frozen source: ownership now answers
            // `false` (rebind landed), so its TTL destroys it.
            metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => "peer_lost_rewind")
                .increment(1);
            let _ = state2.services.host.destroy(new_sandbox_id).await;
            state2.host_registry.invalidate_sandbox(new_sandbox_id);
            if state2
                .services
                .meta
                .transition_session(session_id, SessionState::Evacuating)
                .await
                .is_ok()
            {
                let _ = state2
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: SessionState::Active,
                            to: SessionState::Evacuating,
                            at: chrono::Utc::now(),
                        },
                    )
                    .await;
            }
            let _ = parachute_or_kill(&state2, session_id, durable_row.is_some(), detail).await;
            return;
        }

        // 7b. Release the source (destroy; the export retires with it).
        if let Err(e) = source_backend
            .migration_commit(sandbox_id, &export_id)
            .await
        {
            tracing::warn!(%session_id, %sandbox_id, error = %e,
                "post-copy source commit failed; export TTL will clean up");
        }

        // That's it — durability is deliberately NOT migration's job
        // (operator decision 2026-06-12; supersedes the bookend's
        // "memory durability catch-up" arm). The dest joined the
        // periodic checkpoint cadence at restore like any resumed
        // session; its first periodic capture is a safe Full (no
        // chain), and until that lands recovery rewinds to the
        // source's last periodic row — RPO ≤ cadence, the accepted
        // model. The immediate post-move Full that used to live here
        // cost a guest-visible 2-4s pause seconds after EVERY move
        // (prod 962011bf was its pathological form) to shave a rewind
        // window nobody asked to shave; the teleport's job ends when
        // the guest is live on the dest and the source is released.
        tracing::info!(%session_id,
            "post-copy migration finalized (source released; durability rides the periodic cadence)");
    }
    .instrument(finalize_span),
    );
    Ok(())
}

/// The source host-agent's gRPC address. The heartbeat-warmed pool is
/// authoritative — `hosts.host_addr` in PG is written only at REGISTER,
/// and a host-agent pod that reattaches after a roll doesn't
/// re-register, leaving the PG row pointing at the PREVIOUS pod
/// generation (the prod canary's dest dialed a dead pod-network IP
/// exactly this way). PG is the fallback for a host the pool hasn't
/// warmed since the coordinator's own restart.
async fn source_host_addr(state: &SharedState, host_id: Option<HostId>) -> Option<String> {
    let host_id = host_id?;
    if let Some(addr) = state.services.host_pool.current_addr(host_id) {
        return Some(addr);
    }
    let hosts = state.services.meta.list_active_hosts().await.ok()?;
    hosts.into_iter().find(|h| h.id == host_id)?.host_addr
}

/// Evacuating → Created → Active without relocating (the source VM was
/// aborted back in place; its bindings never changed). Returns false if
/// either transition is refused (the parachute then owns recovery).
/// The parachute's landing depends on whether a durable checkpoint row
/// exists. With one, leave the session `Evacuating` — the scanner
/// rehomes from the row (rung-1 semantics, loss ≤ cadence). WITHOUT
/// one there is nothing to rehome from: leaving `Evacuating` strands a
/// zombie the scanner grinds on forever, so kill the session outright
/// (operator decision, 2026-06-11 — the same call that dropped the
/// first-move row gate).
async fn parachute_or_kill(
    state: &SharedState,
    session_id: SessionId,
    has_durable_row: bool,
    msg: String,
) -> MigrateError {
    if has_durable_row {
        return MigrateError::Parachute(msg);
    }
    tracing::error!(
        %session_id,
        error = %msg,
        "live migration failed past the freeze with NO durable checkpoint \
         row — killing the session (nothing to rehome from)",
    );
    if let Err(e) = state
        .services
        .meta
        .transition_session(session_id, SessionState::Failed)
        .await
    {
        tracing::warn!(%session_id, error = %e, "kill-on-parachute: transition to Failed failed");
    }
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Evacuating,
                to: SessionState::Failed,
                at: chrono::Utc::now(),
            },
        )
        .await;
    MigrateError::Fatal(format!(
        "session lost (no durable checkpoint to rehome from): {msg}"
    ))
}

async fn walk_back_to_active(state: &SharedState, session_id: SessionId) -> bool {
    for target in [SessionState::Created, SessionState::Active] {
        if let Err(e) = state
            .services
            .meta
            .transition_session(session_id, target)
            .await
        {
            tracing::warn!(%session_id, ?target, error = %e,
                "live migration walk-back transition refused");
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::MetadataStore;
    use engram_core::types::session::{Session, SessionMode};
    use engram_core::SandboxId;
    use std::sync::Arc;

    fn active_session() -> Session {
        Session {
            id: SessionId::new(),
            status: SessionState::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "test/repo:live-migrate".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            selected_skills: Vec::new(),
        }
    }

    fn build_state(session: Session) -> (SharedState, Arc<MiniMeta>, HostId) {
        let tmp = std::env::temp_dir().join(format!("live-migrate-test-{}", session.id));
        std::fs::create_dir_all(&tmp).unwrap();
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        // A registered, ready target host (ProcessBackend-backed local
        // client: every migration_* trait method is the default
        // InvalidSpec — the pre-C1 host shape).
        let target = HostId::new();
        let backend: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(
            engram_sandbox_process::ProcessBackend::new(tmp.join("sandboxes")),
        );
        host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(backend)),
        );
        // ADR 0047: placement reads host rows — stage the target as a
        // schedulable host in the mock store.
        meta.add_ready_host(target);
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                tmp.join("blobs"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.join("blobs")),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: tmp,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
            target,
        )
    }

    /// The fallback contract: a fleet that can't do a live move (no
    /// durable row / no host_addr / pre-C1 capture) yields
    /// `Unsupported` — never a frozen guest, never a state change —
    /// and the caller falls back to snapshot-rehome. The session must
    /// be left EXACTLY as found, lease released.
    #[tokio::test]
    async fn unsupported_fleet_falls_back_without_touching_the_session() {
        let session = active_session();
        let session_id = session.id;
        let (state, meta, target) = build_state(session);

        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("pre-C1 fleet must be unsupported");
        assert!(
            matches!(err, MigrateError::Unsupported(_)),
            "got {err:?} — only Unsupported triggers the snapshot-rehome fallback",
        );
        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active, "session untouched");
        // Lease released (Drop spawns a detached DELETE — poll briefly).
        let mut released = false;
        for _ in 0..40 {
            if meta
                .try_acquire_session_lease(session_id, None, "follow-up")
                .await
                .unwrap()
            {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(released, "lease must release after the fallback");
    }

    /// The dest-failure arm: capture succeeds (the source is frozen),
    /// the destination restore fails — the verb ABORTS the source back
    /// (un-pause in place) and walks the session to Active. Zero loss,
    /// AbortedToSource posture.
    #[tokio::test]
    async fn dest_restore_failure_aborts_to_source_and_walks_back_to_active() {
        use engram_core::types::snapshot::SnapshotMetadata;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MigratableFlakyDest {
            abort_called: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::SandboxBackend for MigratableFlakyDest {
            async fn create(
                &self,
                _: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
                Ok(Vec::new())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn snapshot(
                &self,
                _: SandboxId,
            ) -> Result<SnapshotMetadata, engram_core::SandboxError> {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn migration_presetup(
                &self,
                _: SandboxId,
            ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::MigrationPresetupOut {
                    export_id: "test-export".into(),
                    peer_token: "test-token".into(),
                    peer_port: 9102,
                    sidecar_json: b"{}".to_vec(),
                    memory_manifest_json: b"{}".to_vec(),
                    memory_manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                    disk_manifest_ref: None,
                    hot_chunks: vec![],
                })
            }
            async fn migration_capture_postcopy(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::PostCopyCaptureOut {
                    sealed_chunks: 3,
                    total_chunks: 16,
                    pause_ms: 1,
                    disk_drain_ms: 1,
                    vmstate_ms: 1,
                    scan_ms: 1,
                    sealed_disk_chunks: 0,
                    paused_at_unix_ms: 0,
                })
            }
            async fn resume(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                Ok(())
            }
            async fn migration_abort(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<(), engram_core::SandboxError> {
                self.abort_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn restore(
                &self,
                _: SnapshotMetadata,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                // The load-gate marker: the dest provably never ran the
                // shipped state — the coordinator's zero-loss abort arm.
                Err(engram_core::SandboxError::Snapshot(
                    "postcopy-never-loaded: injected dest failure".into(),
                ))
            }
            fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
                std::path::PathBuf::from("/nonexistent")
            }
        }

        let session = active_session();
        let session_id = session.id;
        let source_host = session.host_id.expect("source host");
        let (state, meta, target) = build_state(session);
        // Replace the registry's target backend with the migratable
        // flaky one — same host id, capture-capable, restore-failing.
        let abort_called = Arc::new(AtomicBool::new(false));
        let flaky: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(MigratableFlakyDest {
            abort_called: abort_called.clone(),
        });
        state.host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(flaky.clone())),
        );
        // The SOURCE is resolved through services.host (the registry) by
        // sandbox owner — record the source sandbox's owner as the same
        // flaky backend (it serves capture + abort).
        state
            .host_registry
            .record_sandbox_owner(meta.session.lock().sandbox_id.unwrap(), target);
        // Source host row with an addr + a durable checkpoint row.
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: source_host,
                hostname: "src".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
                ready_images: Vec::new(),
                local_snapshots: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: false,
                capabilities: engram_core::types::host::HostCapabilities::default(),
            });
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: engram_core::types::SnapshotId::new(),
                session_id: Some(session_id),
                host_id: Some(source_host),
                image_version: "test".into(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: None,
                fc_snapshot_version: None,
            });

        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("dest failure must surface");
        assert!(
            matches!(err, MigrateError::AbortedToSource(_)),
            "got {err:?}",
        );
        assert!(
            abort_called.load(Ordering::SeqCst),
            "source must be aborted"
        );
        assert_eq!(
            meta.get_session(session_id).await.unwrap().status,
            SessionState::Active,
            "session walks back to Active (zero loss)",
        );
    }

    /// The commit-routing regression (prod canary 5fa742b7): step 4's
    /// `invalidate_sandbox` + the PG rebind make the OLD sandbox id
    /// unroutable, so a sandbox-routed `migration_commit` lands
    /// "sandbox not found" and the frozen source lingers until the
    /// export TTL. The verb must commit through the source backend
    /// handle it resolved before freezing.
    #[tokio::test]
    async fn commit_reaches_the_frozen_source_after_rebind() {
        use engram_core::types::snapshot::SnapshotMetadata;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MigratableHappyPath {
            commit_called: Arc<AtomicBool>,
            snapshot_called: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::SandboxBackend for MigratableHappyPath {
            async fn create(
                &self,
                _: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
                Ok(Vec::new())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn snapshot(
                &self,
                _: SandboxId,
            ) -> Result<SnapshotMetadata, engram_core::SandboxError> {
                // Durability is the periodic cadence's job, not the
                // finalize's — reaching here means the post-move Full
                // regressed back in.
                self.snapshot_called.store(true, Ordering::SeqCst);
                Err(engram_core::SandboxError::NotFound)
            }
            async fn migration_presetup(
                &self,
                _: SandboxId,
            ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::MigrationPresetupOut {
                    export_id: "test-export".into(),
                    peer_token: "test-token".into(),
                    peer_port: 9102,
                    sidecar_json: b"{}".to_vec(),
                    memory_manifest_json: b"{}".to_vec(),
                    memory_manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                    disk_manifest_ref: None,
                    hot_chunks: vec![],
                })
            }
            async fn migration_capture_postcopy(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::PostCopyCaptureOut {
                    sealed_chunks: 3,
                    total_chunks: 16,
                    pause_ms: 1,
                    disk_drain_ms: 1,
                    vmstate_ms: 1,
                    scan_ms: 1,
                    sealed_disk_chunks: 0,
                    paused_at_unix_ms: 0,
                })
            }
            async fn migration_drain_wait(
                &self,
                _: SandboxId,
            ) -> Result<engram_core::types::snapshot::DrainOutcome, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::DrainOutcome::Done {
                    pulled: 3,
                    alt_sourced: 0,
                    zero_chunks: 0,
                    ms: 5,
                })
            }
            async fn migration_commit(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<(), engram_core::SandboxError> {
                self.commit_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn restore(
                &self,
                _: SnapshotMetadata,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
                std::path::PathBuf::from("/nonexistent")
            }
        }

        let session = active_session();
        let session_id = session.id;
        let source_host = session.host_id.expect("source host");
        let old_sandbox = session.sandbox_id.expect("source sandbox");
        let (state, meta, target) = build_state(session);
        let commit_called = Arc::new(AtomicBool::new(false));
        let snapshot_called = Arc::new(AtomicBool::new(false));
        let happy: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(MigratableHappyPath {
            commit_called: commit_called.clone(),
            snapshot_called: snapshot_called.clone(),
        });
        state.host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(happy.clone())),
        );
        // The source resolves through the recorded sandbox owner (the
        // same fake backend serves both roles, as in the abort test).
        state
            .host_registry
            .record_sandbox_owner(old_sandbox, target);
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: source_host,
                hostname: "src".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
                ready_images: Vec::new(),
                local_snapshots: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: false,
                capabilities: engram_core::types::host::HostCapabilities::default(),
            });
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: engram_core::types::SnapshotId::new(),
                session_id: Some(session_id),
                host_id: Some(source_host),
                image_version: "test".into(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: None,
                fc_snapshot_version: None,
            });

        migrate_session_live(&state, session_id, target)
            .await
            .expect("happy-path migration must succeed");
        // C2: the commit rides the FINALIZE task (after the drain) —
        // poll for it instead of asserting synchronously.
        let mut committed = false;
        for _ in 0..60 {
            if commit_called.load(Ordering::SeqCst) {
                committed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            committed,
            "the frozen source must receive migration_commit (post-drain, \
             via the pre-resolved handle — the old sandbox id is \
             unroutable after the rebind)",
        );
        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(after.host_id, Some(target), "session rebound to dest");
        assert_ne!(
            after.sandbox_id,
            Some(old_sandbox),
            "session points at the new sandbox",
        );
        // Durability is NOT migration's job (operator decision
        // 2026-06-12): the finalize must NOT take a post-move
        // checkpoint or record a row — the dest rides the periodic
        // cadence, and until its first periodic capture, recovery
        // rewinds to the source's last row (RPO ≤ cadence). Settle
        // briefly so a regressed snapshot call (it followed commit in
        // the same task) would have landed before the negative check.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !snapshot_called.load(Ordering::SeqCst),
            "finalize took a post-move checkpoint — the guest-visible \
             post-move pause regressed back in",
        );
        assert_eq!(
            meta.snapshots.lock().len(),
            1,
            "no migration-authored snapshot row; the seeded source row stays the recovery point",
        );
    }

    /// A held lease refuses the migration outright (Fatal, not a
    /// fallback — the rival owns the session right now).
    /// R8: at most one in-flight migration per host endpoint; a second
    /// claim sharing EITHER endpoint refuses, and Drop releases both
    /// (including partial-claim rollback).
    #[test]
    fn migration_gate_claims_both_endpoints_and_releases_on_drop() {
        let (a, b, c) = (HostId::new(), HostId::new(), HostId::new());
        let s1 = SessionId::new();
        let g = MigrationGateGuard::claim(Some(a), b, s1).expect("first claim");
        // Shares the source.
        assert!(MigrationGateGuard::claim(Some(a), c, SessionId::new()).is_none());
        // Shares the dest.
        assert!(MigrationGateGuard::claim(Some(c), b, SessionId::new()).is_none());
        // Disjoint hosts coexist.
        let g2 = MigrationGateGuard::claim(None, c, SessionId::new()).expect("disjoint claim");
        drop(g);
        // Released: both endpoints reusable; the partial-rollback path
        // is exercised by the shares-the-dest refusal above (its `a`
        // claim must have been rolled back).
        let g3 = MigrationGateGuard::claim(Some(a), b, SessionId::new()).expect("after release");
        drop(g2);
        drop(g3);
        // (No global-emptiness assert: the gate is a process-global and
        // sibling tests claim it concurrently; release is proven by the
        // successful re-claim above.)
    }

    /// Regression guard for the rollback-path shard deadlock: the
    /// `Occupied` arm used to `remove` rolled-back claims while still
    /// holding the current host's `entry()` shard lock, which hangs
    /// when two hosts collide on a shard. Run the rollback sequence
    /// enough times with fresh random ids that a same-shard collision
    /// is near-certain — a reintroduced deadlock hangs the whole suite.
    #[test]
    fn claim_rollback_never_deadlocks_on_shard_collision() {
        for _ in 0..5000 {
            let dest = HostId::new();
            let held = MigrationGateGuard::claim(None, dest, SessionId::new()).expect("hold dest");
            // `claim(Some(src), dest, …)` claims src (vacant) then hits
            // dest (occupied) → rolls back src while the dest entry
            // guard is live. If src and dest share a shard, the old
            // code deadlocked here.
            let src = HostId::new();
            assert!(
                MigrationGateGuard::claim(Some(src), dest, SessionId::new()).is_none(),
                "dest is held; the claim must fail and roll back src cleanly",
            );
            // src must be fully released by the rollback — re-claimable.
            let reclaim = MigrationGateGuard::claim(Some(src), HostId::new(), SessionId::new())
                .expect("rolled-back src is reusable");
            drop(reclaim);
            drop(held);
        }
    }

    /// Issue #209 (CASE 2 + CASE 1): a successfully-migrated session must
    /// NOT be abandoned Active-on-the-dest when the finalize's
    /// `migration_drain_wait` keeps returning a transport `Err`. The
    /// pre-fix `Err` arm just logged "leaving the source to its TTL" and
    /// `return`ed — so the source's export TTL fired, the ownership probe
    /// answered owned=false, and the host destroyed the page server the
    /// dest still needed, wedging the session Active on a poisoned VM.
    ///
    /// With the fix the drain is retried with bounded backoff; on
    /// persistent failure the finalize takes the SAME rung-1 rewind as
    /// the PeerLost arm: the dest sandbox is destroyed and the session is
    /// re-armed Evacuating for the scanner to rehome from the durable
    /// row. It also asserts (CASE 1) the teleport_target pin is cleared —
    /// it must never leak past a verb exit, since it strictly pins every
    /// future rehome of the session to the chosen host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_transport_error_rewinds_instead_of_abandoning_active() {
        use engram_core::types::snapshot::SnapshotMetadata;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

        std::env::set_var("ENGRAM_LIVE_TELEPORT", "1");
        // Collapse the retry backoff so the exhaustion path is fast.
        std::env::set_var("ENGRAM_MIGRATION_DRAIN_RETRY_BASE_MS", "1");

        struct DrainFlakyDest {
            destroy_called: Arc<AtomicBool>,
            commit_called: Arc<AtomicBool>,
            drain_attempts: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::SandboxBackend for DrainFlakyDest {
            async fn create(
                &self,
                _: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                self.destroy_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
                Ok(Vec::new())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn snapshot(
                &self,
                _: SandboxId,
            ) -> Result<SnapshotMetadata, engram_core::SandboxError> {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn migration_presetup(
                &self,
                _: SandboxId,
            ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::MigrationPresetupOut {
                    export_id: "test-export".into(),
                    peer_token: "test-token".into(),
                    peer_port: 9102,
                    sidecar_json: b"{}".to_vec(),
                    memory_manifest_json: b"{}".to_vec(),
                    memory_manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                    disk_manifest_ref: None,
                    hot_chunks: vec![],
                })
            }
            async fn migration_capture_postcopy(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, engram_core::SandboxError>
            {
                Ok(engram_core::types::snapshot::PostCopyCaptureOut {
                    sealed_chunks: 3,
                    total_chunks: 16,
                    pause_ms: 1,
                    disk_drain_ms: 1,
                    vmstate_ms: 1,
                    scan_ms: 1,
                    sealed_disk_chunks: 0,
                    paused_at_unix_ms: 0,
                })
            }
            async fn migration_drain_wait(
                &self,
                _: SandboxId,
            ) -> Result<engram_core::types::snapshot::DrainOutcome, engram_core::SandboxError>
            {
                // ALWAYS a transport error — never PeerLost, never Done.
                // The fix must retry, then (budget exhausted) rewind.
                self.drain_attempts.fetch_add(1, Ordering::SeqCst);
                Err(engram_core::SandboxError::Unavailable(
                    "injected transport blip on drain_wait".into(),
                ))
            }
            async fn migration_commit(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<(), engram_core::SandboxError> {
                // The source must NOT be committed (destroyed) when the
                // drain never completed — the rewind owns recovery.
                self.commit_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn restore(
                &self,
                _: SnapshotMetadata,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
                std::path::PathBuf::from("/nonexistent")
            }
        }

        let session = active_session();
        let session_id = session.id;
        let source_host = session.host_id.expect("source host");
        let old_sandbox = session.sandbox_id.expect("source sandbox");
        let (state, meta, target) = build_state(session);
        let destroy_called = Arc::new(AtomicBool::new(false));
        let commit_called = Arc::new(AtomicBool::new(false));
        let drain_attempts = Arc::new(AtomicU32::new(0));
        let flaky: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(DrainFlakyDest {
            destroy_called: destroy_called.clone(),
            commit_called: commit_called.clone(),
            drain_attempts: drain_attempts.clone(),
        });
        state.host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(flaky.clone())),
        );
        state
            .host_registry
            .record_sandbox_owner(old_sandbox, target);
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: source_host,
                hostname: "src".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
                ready_images: Vec::new(),
                local_snapshots: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: false,
                capabilities: engram_core::types::host::HostCapabilities::default(),
            });
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: engram_core::types::SnapshotId::new(),
                session_id: Some(session_id),
                host_id: Some(source_host),
                image_version: "test".into(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: None,
                fc_snapshot_version: None,
            });

        // The verb returns Ok — the move LANDED; the finalize runs async.
        migrate_session_live(&state, session_id, target)
            .await
            .expect("the move lands; the drain finalize runs in the background");

        // The finalize retries the drain, exhausts the budget, and takes
        // the rung-1 rewind: the dest sandbox is destroyed and the
        // session is re-armed Evacuating (durable row present → parachute,
        // not kill). Poll for the terminal posture.
        let mut rewound = false;
        for _ in 0..400 {
            if destroy_called.load(Ordering::SeqCst)
                && meta.get_session(session_id).await.unwrap().status == SessionState::Evacuating
            {
                rewound = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            rewound,
            "issue #209: a persistent drain transport error must rewind \
             (destroy dest + re-arm Evacuating), never abandon the session \
             Active on the dest while the source page server is reaped",
        );
        assert!(
            drain_attempts.load(Ordering::SeqCst) > 1,
            "the drain must be RETRIED before giving up, not abandoned on \
             the first transport error (saw {} attempt(s))",
            drain_attempts.load(Ordering::SeqCst),
        );
        assert!(
            !commit_called.load(Ordering::SeqCst),
            "the source must NOT be committed/destroyed when the drain never \
             completed — the rewind owns recovery from the durable row",
        );
        // CASE 1: the teleport_target pin must be cleared — it must never
        // outlive the verb (it strictly pins every future rehome).
        assert_eq!(
            meta.get_teleport_target(session_id).await.unwrap(),
            None,
            "issue #209 CASE 1: the teleport_target pin leaked past the verb",
        );
    }

    #[tokio::test]
    async fn held_lease_refuses_migration() {
        let session = active_session();
        let session_id = session.id;
        let (state, meta, target) = build_state(session);
        assert!(meta
            .try_acquire_session_lease(session_id, None, "rival")
            .await
            .unwrap());
        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("held lease must refuse");
        assert!(matches!(err, MigrateError::Fatal(_)));
    }
}
