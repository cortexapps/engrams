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

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::{SandboxId, SessionId};

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
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
    // 0108: bind via the production fused path, never on a Pending row.
    meta.transition_session_created(id, sandbox)
        .await
        .expect("pending->created");
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
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
    meta.transition_session(id, SessionState::Evicting, BindingDisposition::Retain)
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
    meta.transition_session(id, SessionState::Idle, BindingDisposition::Detach)
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
    meta.transition_session(id, SessionState::Created, BindingDisposition::Retain)
        .await
        .expect("idle->created (resume)");
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("created->active");
    meta.transition_session(id, SessionState::Evicting, BindingDisposition::Retain)
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
    meta.transition_session(id, SessionState::Evicting, BindingDisposition::Retain)
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
    meta.transition_session(id, SessionState::Evicting, BindingDisposition::Retain)
        .await
        .expect("active->evicting");
    meta.transition_session(id, SessionState::HostLost, BindingDisposition::Retain)
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

/// #584 review regression (the recurring missing-comma projection class):
/// `get_session` must round-trip park_rung, parked_at, AND the live disk
/// manifest under their OWN column names. A dropped comma in its SELECT
/// silently aliases a neighbor (`x AS park_rung`), and row.rs's tolerant
/// decode masks it (`unwrap_or(0)` / `.ok().flatten()`): a rung-2
/// parked-paused session then reads rung 0 — the un-pause ascent flips a
/// FROZEN VM to Active without resuming it — and the disk-only cold-boot
/// gate never fires. Only a live-PG round-trip catches this shape.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn get_session_round_trips_park_and_live_disk_manifest() {
    let Some(meta) = pg().await else { return };
    let (id, sandbox) = seed_active(&meta).await;

    let parked_at = chrono::Utc::now();
    meta.set_session_park_rung(id, 2, Some(parked_at))
        .await
        .expect("set park_rung");
    let mref = engram_core::types::manifest::ManifestRef {
        manifest_id: uuid::Uuid::new_v4(),
        version: 7,
    };
    meta.update_live_disk_manifest(id, sandbox, mref)
        .await
        .expect("publish live disk manifest");

    let s = meta.get_session(id).await.expect("get_session");
    assert_eq!(s.park_rung, 2, "park_rung must project under its own name");
    assert!(
        s.parked_at.is_some(),
        "parked_at must project under its own name"
    );
    assert_eq!(
        s.live_disk_manifest,
        Some(mref),
        "live_disk_manifest_{{id,version}} must project under their own names"
    );
}
