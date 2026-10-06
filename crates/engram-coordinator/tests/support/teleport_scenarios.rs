//! The same scripted phase scenarios run with SimMetadataStore and Postgres.
#[path = "teleport.rs"]
pub(super) mod support;
use super::coordinator;
use coordinator::session_ops::OpOutcome;
use engram_core::types::teleport::{SourceRelease, TeleportPatch, TeleportPhase};
use engram_core::types::SessionState;
use std::sync::{atomic::Ordering, Arc};
use support::Rig;

macro_rules! scenario {
    ($name:ident) => {
        mod $name {
            use super::*;
            #[tokio::test]
            async fn sim() {
                super::$name(Rig::sim().await).await;
            }
            #[tokio::test]
            #[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
            async fn pg() {
                let Some(db) = engram_testkit::pg::fresh_db().await else {
                    return;
                };
                let clock = engram_sim::ManualClock::new();
                let meta = Arc::new(db.store.clone().with_clock(clock.clone()));
                super::$name(Rig::new(meta, clock).await).await;
            }
        }
    };
}
async fn snapshot_teleport_end_to_end(rig: Rig) {
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.phase().await, None);
    let session = rig
        .state
        .services
        .meta
        .get_session(rig.row.session_id)
        .await
        .unwrap();
    assert_eq!(session.status, SessionState::Active);
    assert_eq!(session.host_id, Some(rig.row.dest_host_id));
    assert_eq!(session.sandbox_id, Some(rig.dest.sandbox));
    assert_eq!(rig.source.captures.load(Ordering::SeqCst), 1);
    assert_eq!(rig.dest.restores.load(Ordering::SeqCst), 1);
    assert_eq!(rig.source.destroys.load(Ordering::SeqCst), 1);
    assert_eq!(rig.dest.spawns.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.state
            .services
            .meta
            .sandbox_tombstones_for_host(rig.row.source_host_id)
            .await
            .unwrap(),
        vec![rig.source.sandbox]
    );
}
scenario!(snapshot_teleport_end_to_end);

async fn rollback_pending_until_source_resume_acks(rig: Rig) {
    rig.source.capture_fails.store(true, Ordering::SeqCst);
    rig.source.resume_fails.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
    assert_eq!(rig.phase().await, Some(TeleportPhase::RollingBack));
    assert_eq!(
        rig.state
            .services
            .meta
            .get_session(rig.row.session_id)
            .await
            .unwrap()
            .status,
        SessionState::Evacuating
    );
    rig.source.resume_fails.store(false, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.phase().await, None);
    assert_eq!(
        rig.state
            .services
            .meta
            .get_session(rig.row.session_id)
            .await
            .unwrap()
            .status,
        SessionState::Active
    );
    assert_eq!(rig.source.resumes.load(Ordering::SeqCst), 2);
}
scenario!(rollback_pending_until_source_resume_acks);

async fn source_dead_at_release_is_source_host_gone(rig: Rig) {
    rig.source.destroy_fails.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
    assert_eq!(rig.phase().await, Some(TeleportPhase::Attached));
    assert!(!rig
        .state
        .services
        .meta
        .teleport_release_source(
            rig.row.id,
            rig.op.epoch.unwrap(),
            SourceRelease::SourceHostGone
        )
        .await
        .unwrap());
    rig.state
        .services
        .meta
        .mark_host_dead_if_lease_expired(rig.row.source_host_id)
        .await
        .unwrap();
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.phase().await, None);
}
scenario!(source_dead_at_release_is_source_host_gone);

async fn dead_host_sweep_skips_machine_owned_sources(rig: Rig) {
    assert!(rig
        .state
        .services
        .meta
        .mark_host_dead_if_lease_expired(rig.row.source_host_id)
        .await
        .unwrap()
        .is_empty());
    let session = rig
        .state
        .services
        .meta
        .get_session(rig.row.session_id)
        .await
        .unwrap();
    assert_eq!(session.status, SessionState::Evacuating);
    assert_eq!(session.sandbox_id, Some(rig.source.sandbox));
}
scenario!(dead_host_sweep_skips_machine_owned_sources);

async fn fenced_step_stops_silently(mut rig: Rig) {
    rig.op.epoch = Some(rig.op.epoch.unwrap() + 1);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.source.captures.load(Ordering::SeqCst), 0);
    assert_eq!(rig.phase().await, Some(TeleportPhase::Admitted));
}
scenario!(fenced_step_stops_silently);

async fn commit_conflict_rolls_back_dest(rig: Rig) {
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
    meta.assign_session_host(rig.row.session_id, Some(rig.row.dest_host_id))
        .await
        .unwrap();
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.dest.destroys.load(Ordering::SeqCst), 1);
    assert_eq!(rig.phase().await, None);
}
scenario!(commit_conflict_rolls_back_dest);

async fn crash_at_every_phase_is_resumed_by_successor(mut rig: Rig, phase: TeleportPhase) {
    use engram_core::traits::HostClient;
    use engram_core::types::{snapshot::SnapshotRecord, BindingDisposition};
    let meta = &rig.state.services.meta;
    let epoch = rig.op.epoch.unwrap();
    let fence = engram_core::traits::SessionFence {
        session_id: rig.row.session_id,
        epoch: epoch.try_into().unwrap(),
    };
    if matches!(
        phase,
        TeleportPhase::Captured
            | TeleportPhase::Restored
            | TeleportPhase::Committed
            | TeleportPhase::Attached
    ) {
        let snapshot = rig
            .source
            .snapshot_hold(rig.source.sandbox, fence)
            .await
            .unwrap();
        meta.fenced_record_snapshot(
            SnapshotRecord {
                id: snapshot.id,
                session_id: Some(rig.row.session_id),
                host_id: Some(rig.row.source_host_id),
                image_version: snapshot.image_version.clone(),
                size_bytes: snapshot.size_bytes,
                created_at: snapshot.created_at,
                last_accessed_at: snapshot.created_at,
                disk_manifest: None,
                memory_manifest: None,
                recoverable: false,
                aux_bundles: vec![],
                events_cursor: None,
                fc_snapshot_version: None,
            },
            epoch,
        )
        .await
        .unwrap();
        meta.teleport_advance(
            rig.row.id,
            TeleportPhase::Admitted,
            TeleportPhase::Captured,
            TeleportPatch {
                snapshot_id: Some(snapshot.id),
                ..Default::default()
            },
            epoch,
        )
        .await
        .unwrap();
        if phase != TeleportPhase::Captured {
            let dest = rig.dest.restore(snapshot, fence).await.unwrap();
            meta.teleport_advance(
                rig.row.id,
                TeleportPhase::Captured,
                TeleportPhase::Restored,
                TeleportPatch {
                    dest_sandbox_id: Some(dest),
                    ..Default::default()
                },
                epoch,
            )
            .await
            .unwrap();
        }
        if matches!(phase, TeleportPhase::Committed | TeleportPhase::Attached) {
            assert!(meta
                .teleport_commit(rig.row.id, epoch)
                .await
                .unwrap()
                .is_some());
            rig.state
                .host_registry
                .record_sandbox_owner(rig.dest.sandbox, rig.row.dest_host_id);
        }
        if phase == TeleportPhase::Attached {
            meta.fenced_transition_session(
                rig.row.session_id,
                epoch,
                SessionState::Active,
                BindingDisposition::Retain,
            )
            .await
            .unwrap();
            meta.teleport_advance(
                rig.row.id,
                TeleportPhase::Committed,
                TeleportPhase::Attached,
                TeleportPatch::default(),
                epoch,
            )
            .await
            .unwrap();
        }
    } else if phase == TeleportPhase::RollingBack {
        rig.source.capture_fails.store(true, Ordering::SeqCst);
        rig.source.resume_fails.store(true, Ordering::SeqCst);
        assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
        rig.source.resume_fails.store(false, Ordering::SeqCst);
    }
    assert_eq!(rig.phase().await, Some(phase));
    rig.reclaim().await;
    assert!(
        matches!(rig.drive().await, OpOutcome::Done),
        "phase {phase:?}"
    );
    assert_eq!(rig.phase().await, None, "phase {phase:?}");
    let session = meta_session(&rig).await;
    assert_eq!(session.status, SessionState::Active);
    assert_eq!(
        session.host_id,
        Some(if phase == TeleportPhase::RollingBack {
            rig.row.source_host_id
        } else {
            rig.row.dest_host_id
        })
    );
    assert_eq!(
        rig.source.captures.load(Ordering::SeqCst),
        1,
        "no second capture after {phase:?}"
    );
}
async fn meta_session(rig: &Rig) -> engram_core::types::Session {
    rig.state
        .services
        .meta
        .get_session(rig.row.session_id)
        .await
        .unwrap()
}
mod crash_at_every_phase {
    use super::*;
    const PHASES: [TeleportPhase; 6] = [
        TeleportPhase::Admitted,
        TeleportPhase::Captured,
        TeleportPhase::Restored,
        TeleportPhase::Committed,
        TeleportPhase::Attached,
        TeleportPhase::RollingBack,
    ];
    #[tokio::test]
    async fn sim() {
        for phase in PHASES {
            crash_at_every_phase_is_resumed_by_successor(Rig::sim().await, phase).await;
        }
    }
    #[tokio::test]
    #[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
    async fn pg() {
        for phase in PHASES {
            let Some(db) = engram_testkit::pg::fresh_db().await else {
                return;
            };
            let clock = engram_sim::ManualClock::new();
            let meta = Arc::new(db.store.clone().with_clock(clock.clone()));
            crash_at_every_phase_is_resumed_by_successor(Rig::new(meta, clock).await, phase).await;
        }
    }
}

async fn crash_after_presetup_reuses_live_payload(mut rig: Rig) {
    rig.make_live().await;
    rig.source.block_capture.store(true, Ordering::SeqCst);
    let task = rig.spawn_drive();
    rig.source.capture_entered.notified().await;
    let row = rig
        .state
        .services
        .meta
        .open_teleport_for_session(rig.row.session_id)
        .await
        .unwrap()
        .unwrap();
    let payload = row.live_payload.unwrap();
    assert_eq!(row.export_id.as_deref(), Some("export-one"));
    assert_eq!(payload["migration_source"]["peer_token"], "token-one");
    assert_eq!(payload["migration_source"]["peer_addr"], "source:9000");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    rig.reclaim().await;
    rig.source.block_capture.store(false, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.source.presetups.load(Ordering::SeqCst), 1);
    assert_eq!(rig.phase().await, None);
}
scenario!(crash_after_presetup_reuses_live_payload);

async fn live_refusal_downgrades_to_snapshot_in_row(rig: Rig) {
    rig.make_live().await;
    rig.source.live_refused.store(true, Ordering::SeqCst);
    rig.source.destroy_fails.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
    let row = rig
        .state
        .services
        .meta
        .open_teleport_for_session(rig.row.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.kind,
        engram_core::types::teleport::TeleportKind::Snapshot
    );
    assert_eq!(row.phase, TeleportPhase::Attached);
    assert_eq!(rig.source.captures.load(Ordering::SeqCst), 1);
}
scenario!(live_refusal_downgrades_to_snapshot_in_row);

async fn lost_live_export_rolls_back_with_durable_error(rig: Rig) {
    rig.make_live().await;
    rig.source.live_lost.store(true, Ordering::SeqCst);
    rig.source.resume_fails.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
    let row = rig
        .state
        .services
        .meta
        .open_teleport_for_session(rig.row.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.phase, TeleportPhase::RollingBack);
    assert_eq!(row.error.as_deref(), Some("live_export_lost"));
    assert_eq!(rig.source.presetups.load(Ordering::SeqCst), 1);
    // A live rollback releases the fenced export through migration_abort,
    // never through a plain resume, and stays pending until the source
    // acknowledges it.
    assert_eq!(rig.source.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(rig.source.resumes.load(Ordering::SeqCst), 0);
    rig.source.resume_fails.store(false, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.source.aborts.load(Ordering::SeqCst), 2);
    assert_eq!(rig.source.resumes.load(Ordering::SeqCst), 0);
    // A finished row is no longer open.
    assert_eq!(rig.phase().await, None);
    assert_eq!(
        meta_session(&rig).await.status,
        engram_core::types::SessionState::Active
    );
}
scenario!(lost_live_export_rolls_back_with_durable_error);

/// A consumed export (an earlier abort passed its point of no return) is
/// not a resumed guest: the rollback still needs the resume ack before it
/// declares Active.
async fn consumed_live_export_rollback_waits_for_the_resume_ack(rig: Rig) {
    rig.make_live().await;
    rig.source.live_lost.store(true, Ordering::SeqCst);
    rig.source.abort_not_found.store(true, Ordering::SeqCst);
    rig.source.resume_fails.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::RetryAfter(_, _)));
    assert_eq!(rig.phase().await, Some(TeleportPhase::RollingBack));
    assert_eq!(rig.source.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(rig.source.resumes.load(Ordering::SeqCst), 1);
    assert_eq!(
        meta_session(&rig).await.status,
        engram_core::types::SessionState::Evacuating
    );
    rig.source.resume_fails.store(false, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.source.resumes.load(Ordering::SeqCst), 2);
    assert_eq!(rig.phase().await, None);
    assert_eq!(
        meta_session(&rig).await.status,
        engram_core::types::SessionState::Active
    );
}
scenario!(consumed_live_export_rollback_waits_for_the_resume_ack);

/// A source sandbox that no longer exists cannot be returned to: the
/// rollback settles the session honestly instead of retrying forever.
async fn rollback_with_a_destroyed_source_fails_the_move(rig: Rig) {
    rig.source.capture_fails.store(true, Ordering::SeqCst);
    rig.source.resume_not_found.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.phase().await, None);
    let events = rig
        .state
        .services
        .meta
        .list_session_events_since(rig.row.session_id, -1, 100)
        .await
        .unwrap();
    let terminal: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "teleport_finished")
        .collect();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].payload["outcome"], "failed");
    assert_eq!(terminal[0].payload["error"], "source_lost_during_rollback");
    // No durable snapshot in this rig: the session is Dead, not Idle.
    assert_eq!(
        meta_session(&rig).await.status,
        engram_core::types::SessionState::Dead
    );
}
scenario!(rollback_with_a_destroyed_source_fails_the_move);

/// A disk-only session (live disk manifest, no memory snapshot) is still
/// recoverable through the cold boot: a lost source settles it Idle, not
/// Dead, and keeps the manifest.
async fn rollback_with_a_destroyed_source_keeps_a_disk_only_session_idle(rig: Rig) {
    let manifest = engram_core::types::manifest::ManifestRef::new();
    rig.state
        .services
        .meta
        .update_live_disk_manifest(rig.row.session_id, rig.row.source_sandbox_id, manifest)
        .await
        .unwrap();
    rig.source.capture_fails.store(true, Ordering::SeqCst);
    rig.source.resume_not_found.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert_eq!(rig.phase().await, None);
    let session = meta_session(&rig).await;
    assert_eq!(session.status, engram_core::types::SessionState::Idle);
    assert_eq!(session.sandbox_id, None);
    assert_eq!(session.live_disk_manifest, Some(manifest));
}
scenario!(rollback_with_a_destroyed_source_keeps_a_disk_only_session_idle);

async fn another_admission(rig: &Rig) -> engram_core::types::teleport::TeleportAdmitRequest {
    use engram_core::types::teleport::*;
    let meta = &rig.state.services.meta;
    let sid = meta
        .create_session(engram_core::types::SessionSpec {
            image: "test:teleport".into(),
            mode: engram_core::types::SessionMode::DevVm,
        })
        .await
        .unwrap();
    meta.assign_session_host(sid, Some(rig.row.source_host_id))
        .await
        .unwrap();
    meta.transition_session_created(
        sid,
        engram_core::SandboxId::from(rig.state.services.entropy.uuid()),
    )
    .await
    .unwrap();
    meta.transition_session(
        sid,
        SessionState::Active,
        engram_core::types::BindingDisposition::Retain,
    )
    .await
    .unwrap();
    TeleportAdmitRequest {
        id: engram_core::TeleportId::from(rig.state.services.entropy.uuid()),
        session_id: sid,
        reason: TeleportReason::Ui,
        epoch: 0,
        candidates: vec![rig.row.dest_host_id],
        pinned_dest: Some(rig.row.dest_host_id),
        mem_budget_mib: 128,
        cpu_budget_vcpus: 1,
        max_open_per_dest: 1,
        live_capable: false,
    }
}
async fn admit_no_fit_leaves_session_active_and_no_row(rig: Rig) {
    let req = another_admission(&rig).await;
    let sid = req.session_id;
    assert!(matches!(
        rig.state.services.meta.teleport_admit(req).await.unwrap(),
        engram_core::types::teleport::TeleportAdmitOutcome::NoFit
    ));
    assert_eq!(
        rig.state
            .services
            .meta
            .get_session(sid)
            .await
            .unwrap()
            .status,
        SessionState::Active
    );
    assert!(rig
        .state
        .services
        .meta
        .open_teleport_for_session(sid)
        .await
        .unwrap()
        .is_none());
}
scenario!(admit_no_fit_leaves_session_active_and_no_row);

async fn two_teleports_to_one_dest_respect_max_open(rig: Rig) {
    let mut left = another_admission(&rig).await;
    let mut right = another_admission(&rig).await;
    left.max_open_per_dest = 2;
    right.max_open_per_dest = 2;
    let meta = &rig.state.services.meta;
    let (a, b) = tokio::join!(meta.teleport_admit(left), meta.teleport_admit(right));
    use engram_core::types::teleport::TeleportAdmitOutcome;
    assert_eq!(
        [a.unwrap(), b.unwrap()]
            .iter()
            .filter(|r| matches!(r, TeleportAdmitOutcome::Admitted(_)))
            .count(),
        1
    );
    assert_eq!(meta.list_open_teleports().await.unwrap().len(), 2);
}
scenario!(two_teleports_to_one_dest_respect_max_open);

async fn retire_host_grants_only_after_empty_heartbeat_then_delete_host_ok(rig: Rig) {
    use engram_core::types::host::{
        CordonOwner, DeleteHostOutcome, HostHeartbeat, RetirementGrant,
    };
    let meta = &rig.state.services.meta;
    let source = rig.row.source_host_id;
    meta.request_host_retirement(
        source,
        CordonOwner::Admin,
        "test",
        rig.state.services.clock.now_utc(),
    )
    .await
    .unwrap();
    assert!(matches!(
        meta.grant_host_retirement(source, rig.state.services.clock.now_utc())
            .await
            .unwrap(),
        RetirementGrant::Blocked(_)
    ));
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    assert!(matches!(
        meta.grant_host_retirement(source, rig.state.services.clock.now_utc())
            .await
            .unwrap(),
        RetirementGrant::Blocked(_)
    ));
    rig.clock.advance(std::time::Duration::from_secs(1));
    let h = meta.get_host(source).await.unwrap().unwrap();
    meta.touch_host_heartbeat(
        source,
        HostHeartbeat {
            status: h.status,
            capacity: h.capacity,
            utilization: h.utilization,
            ready_images: vec![],
            current_bundles: vec![],
            sandbox_bundles: vec![],
            total_vcpus: h.total_vcpus,
            wire_version: h.wire_version,
            stages_images: false,
            capabilities: h.capabilities,
            lease_renew_until: None,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(
            meta.grant_host_retirement(source, rig.state.services.clock.now_utc())
                .await
                .unwrap(),
            RetirementGrant::Blocked(_)
        ),
        "tombstone still blocks grant"
    );
    meta.ack_sandbox_tombstones_by_absence(source, &[])
        .await
        .unwrap();
    coordinator::teleport::run_once(&Default::default(), &rig.state)
        .await
        .unwrap();
    assert_eq!(
        meta.get_host(source).await.unwrap().unwrap().status,
        engram_core::types::HostStatus::Retired
    );
    assert_eq!(
        meta.delete_host(source).await.unwrap(),
        DeleteHostOutcome::Deleted
    );
    assert_eq!(
        meta.delete_host(source).await.unwrap(),
        DeleteHostOutcome::Deleted
    );
}
scenario!(retire_host_grants_only_after_empty_heartbeat_then_delete_host_ok);

async fn live_peer_lost_fails_to_idle_with_durable_row(rig: Rig) {
    rig.make_live().await;
    let meta = &rig.state.services.meta;
    let now = rig.state.services.clock.now_utc();
    meta.fenced_record_snapshot(
        engram_core::types::snapshot::SnapshotRecord {
            id: rig.source.snapshot,
            session_id: Some(rig.row.session_id),
            host_id: Some(rig.row.source_host_id),
            image_version: "test".into(),
            size_bytes: 1,
            created_at: now,
            last_accessed_at: now,
            disk_manifest: None,
            memory_manifest: None,
            recoverable: true,
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        },
        rig.op.epoch.unwrap(),
    )
    .await
    .unwrap();
    rig.dest.peer_lost.store(true, Ordering::SeqCst);
    assert!(matches!(rig.drive().await, OpOutcome::Done));
    let s = meta.get_session(rig.row.session_id).await.unwrap();
    assert_eq!(s.status, SessionState::Idle);
    assert_eq!(s.sandbox_id, None);
    assert_eq!(rig.phase().await, None);
    assert_eq!(rig.dest.destroys.load(Ordering::SeqCst), 1);
    let events = meta
        .list_session_events_since(rig.row.session_id, -1, 100)
        .await
        .unwrap();
    let terminal: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "teleport_finished")
        .collect();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].payload["outcome"], "failed");
    assert_eq!(terminal[0].payload["error"], "peer_lost");
}
scenario!(live_peer_lost_fails_to_idle_with_durable_row);
