//! Live-Postgres integration tests for ADR 0034's metadata surface:
//! the `evicting` status (migration 0050's CHECK swap), the
//! `evict_attempts` counter (reset-on-entry + atomic bump), the
//! eviction-scanner sweep query, and the L3 detection-backstop query.
//!
//! The PG side is what these tests exercise — the queries must work
//! against the real schema + legality-table-enforcing
//! `transition_session`, not a mock.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the
//! Postgres-gated-ignored lane alongside `admin_evac_live_pg`.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::{SandboxId, SessionId};

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: ENGRAM_TEST_DATABASE_URL not set. Run with `just db-up` first; \
                 default URL is postgres://engram:engram@localhost:5435/engram",
            );
            return None;
        }
    };
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
}

/// Create a session and walk it to Active with a bound sandbox, the
/// way the create handler does — through `transition_session`, no
/// escape hatch.
async fn seed_active(meta: &Arc<dyn MetadataStore>) -> (SessionId, SandboxId) {
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm-evict-test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    let sandbox = SandboxId::new();
    meta.assign_session_sandbox(id, Some(sandbox))
        .await
        .expect("bind sandbox");
    meta.transition_session(id, SessionState::Created)
        .await
        .expect("pending->created");
    meta.transition_session(id, SessionState::Active)
        .await
        .expect("created->active");
    (id, sandbox)
}

/// The full Evicting round-trip: Active → Evicting passes the 0050
/// CHECK constraint, entry resets `evict_attempts`, bumps are atomic
/// and returned, the sweep query sees the row, and the pipeline's
/// terminal Evicting → Idle lands.
#[tokio::test]
#[ignore]
async fn evicting_round_trip_counter_and_sweep() {
    let Some(meta) = pg().await else { return };
    let (id, _sandbox) = seed_active(&meta).await;

    // Pre-dirty the counter via a previous eviction cycle's bumps:
    // enter Evicting, bump twice, leave via Idle, resume to Active.
    meta.transition_session(id, SessionState::Evicting)
        .await
        .expect("active->evicting (0050 CHECK must allow it)");
    assert_eq!(meta.bump_evict_attempts(id).await.expect("bump1"), 1);
    assert_eq!(meta.bump_evict_attempts(id).await.expect("bump2"), 2);

    // Sweep sees the row with its current attempts count.
    let listed = meta.list_evicting_sessions().await.expect("list");
    let mine = listed
        .iter()
        .find(|(s, _)| s.id == id)
        .expect("swept row present");
    assert_eq!(mine.1, 2, "sweep must carry the bumped count");

    // Pipeline terminal transition.
    meta.transition_session(id, SessionState::Idle)
        .await
        .expect("evicting->idle");
    assert!(
        !meta
            .list_evicting_sessions()
            .await
            .expect("list")
            .iter()
            .any(|(s, _)| s.id == id),
        "Idle row must leave the sweep"
    );

    // Re-entry resets the budget: resume to Active, evict again.
    meta.transition_session(id, SessionState::Created)
        .await
        .expect("idle->created (resume)");
    meta.transition_session(id, SessionState::Active)
        .await
        .expect("created->active");
    meta.transition_session(id, SessionState::Evicting)
        .await
        .expect("active->evicting again");
    assert_eq!(
        meta.bump_evict_attempts(id)
            .await
            .expect("bump after reset"),
        1,
        "transition_session(Evicting) must reset evict_attempts to 0"
    );
}

/// Evicting sessions must appear in `list_active_sessions` with the
/// sandbox binding intact — the eviction scanner re-picks them after a
/// coord roll and its guard reads `sessions.sandbox_id` directly
/// (ADR 0047, no in-memory registry). When `evicting` was missing from
/// the status filter, a coord roll mid-eviction dropped the session
/// from the active set; the scanner never re-picked it and the budget
/// exhausted into a spurious HostLost with the VM still running
/// (prod session 5cfb90b8, 2026-06-03).
#[tokio::test]
#[ignore]
async fn evicting_sessions_listed_active_with_binding() {
    let Some(meta) = pg().await else { return };
    let (id, sandbox) = seed_active(&meta).await;
    meta.transition_session(id, SessionState::Evicting)
        .await
        .expect("active->evicting");

    let listed = meta.list_active_sessions().await.expect("list");
    let row = listed
        .iter()
        .find(|s| s.id == id)
        .expect("evicting session must be in list_active_sessions");
    assert_eq!(
        row.sandbox_id,
        Some(sandbox),
        "evicting row must keep its sandbox binding (repopulate_routing rebinds from it)",
    );
}

/// Budget-exhaustion fallback edge: Evicting → HostLost must pass the
/// legality table AND the live CHECK constraint.
#[tokio::test]
#[ignore]
async fn evicting_falls_back_to_host_lost() {
    let Some(meta) = pg().await else { return };
    let (id, _sandbox) = seed_active(&meta).await;
    meta.transition_session(id, SessionState::Evicting)
        .await
        .expect("active->evicting");
    meta.transition_session(id, SessionState::HostLost)
        .await
        .expect("evicting->host_lost (budget exhaustion fallback)");
}

/// The L3 backstop query: an Active session with a bound sandbox is
/// returned once its newest event (here: none — COALESCE to the row's
/// created_at) is older than the TTL; a generous TTL excludes it; a
/// session without a bound sandbox never appears.
#[tokio::test]
#[ignore]
async fn backstop_query_filters_on_ttl_and_sandbox() {
    let Some(meta) = pg().await else { return };
    let (id, sandbox) = seed_active(&meta).await;

    // TTL 0: "idle for at least 0s" — matches immediately.
    let stale = meta
        .list_active_sessions_idle_past(0)
        .await
        .expect("backstop ttl=0");
    let mine = stale.iter().find(|(sid, _, _)| *sid == id);
    let (_, got_sandbox, _) = mine.expect("active+bound session past ttl=0 must appear");
    assert_eq!(*got_sandbox, sandbox);

    // Generous TTL: a seconds-old session is not idle-past-1h.
    assert!(
        !meta
            .list_active_sessions_idle_past(3600)
            .await
            .expect("backstop ttl=1h")
            .iter()
            .any(|(sid, _, _)| *sid == id),
        "fresh session must not appear at ttl=1h"
    );

    // Unbinding the sandbox removes it from the backstop's view
    // (nothing to evict; other lifecycle paths own bare rows).
    meta.assign_session_sandbox(id, None).await.expect("unbind");
    assert!(
        !meta
            .list_active_sessions_idle_past(0)
            .await
            .expect("backstop after unbind")
            .iter()
            .any(|(sid, _, _)| *sid == id),
        "sandbox-less session must not appear"
    );
}

/// Events move the backstop's clock: after appending a fresh event,
/// the session's `last_event_at` is the event time, so it again fails
/// a generous TTL even if the row itself were old.
#[tokio::test]
#[ignore]
async fn backstop_query_uses_newest_event() {
    let Some(meta) = pg().await else { return };
    let (id, _sandbox) = seed_active(&meta).await;
    meta.append_session_event(id, "harness_idle", serde_json::json!({}))
        .await
        .expect("append event");
    let stale = meta
        .list_active_sessions_idle_past(0)
        .await
        .expect("backstop ttl=0");
    let (_, _, last_event_at) = stale
        .iter()
        .find(|(sid, _, _)| *sid == id)
        .expect("present at ttl=0");
    // The reported watermark must be >= the session's creation time —
    // i.e. it tracked the appended event, not a NULL-join artifact.
    let session = meta.get_session(id).await.expect("get");
    assert!(
        *last_event_at >= session.created_at,
        "last_event_at must reflect the appended event"
    );
}

/// ADR 0045 D5 / issue #147: `touch_session_lease` refreshes only the
/// holder's own row — a touch can't resurrect a reaped/foreign lease,
/// and a touched lease survives the stale-reap window.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn touch_session_lease_is_holder_scoped_and_defeats_the_reaper() {
    let Some(meta) = pg().await else { return };
    let session_id = SessionId::new();

    assert!(meta
        .try_acquire_session_lease(session_id, None, "pod-a")
        .await
        .unwrap());

    // Holder touch succeeds; a foreign pod's touch does not.
    assert!(meta.touch_session_lease(session_id, "pod-a").await.unwrap());
    assert!(!meta.touch_session_lease(session_id, "pod-b").await.unwrap());

    // A touched lease is NOT reaped at a max_age its refresh stays inside.
    assert!(meta.touch_session_lease(session_id, "pod-a").await.unwrap());
    let reaped = meta
        .sweep_stale_session_leases(std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        !reaped.iter().any(|l| l.session_id == session_id),
        "freshly-touched lease must survive the reaper"
    );

    // After release, touch reports the loss.
    assert!(
        meta.release_session_lease(session_id, "pod-a")
            .await
            .unwrap(),
        "holder release deletes its own row"
    );
    assert!(!meta.touch_session_lease(session_id, "pod-a").await.unwrap());
}

/// Issue #212: `release_session_lease` is holder-scoped — a reaped holder
/// whose row was re-acquired by a new holder must NOT blind-delete the new
/// holder's lease on its (late) Drop. Without the `AND locked_by = $2`
/// filter the fleet's primary serializer fails open after any >180s hold.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn release_session_lease_is_holder_scoped_no_blind_delete() {
    let Some(meta) = pg().await else { return };
    let session_id = SessionId::new();

    // Holder A acquires.
    assert!(meta
        .try_acquire_session_lease(session_id, None, "pod-a")
        .await
        .unwrap());

    // A is reaped (it held >180s without touching). Simulate with a
    // zero-age sweep that deletes everything currently held.
    let reaped = meta
        .sweep_stale_session_leases(std::time::Duration::from_secs(0))
        .await
        .unwrap();
    assert!(
        reaped.iter().any(|l| l.session_id == session_id),
        "the reaper must have removed A's aged lease"
    );

    // Holder B legitimately acquires the now-free lease and starts its
    // own pipeline.
    assert!(meta
        .try_acquire_session_lease(session_id, None, "pod-b")
        .await
        .unwrap());

    // A's RPC finally resolves and its guard drops, firing the release.
    // The scoped DELETE must NOT touch B's row: it affects 0 rows.
    assert!(
        !meta
            .release_session_lease(session_id, "pod-a")
            .await
            .unwrap(),
        "A's late release must affect 0 rows — it no longer owns the lease",
    );

    // B's lease is still present, so a third holder C cannot acquire it.
    assert!(
        meta.session_lease_held(session_id).await.unwrap(),
        "B's lease must survive A's blind release",
    );
    assert!(
        !meta
            .try_acquire_session_lease(session_id, None, "pod-c")
            .await
            .unwrap(),
        "C must NOT acquire while B holds the lease",
    );

    // B can still touch and release its own lease.
    assert!(meta.touch_session_lease(session_id, "pod-b").await.unwrap());
    assert!(meta
        .release_session_lease(session_id, "pod-b")
        .await
        .unwrap());
}
