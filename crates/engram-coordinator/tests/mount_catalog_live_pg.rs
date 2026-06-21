//! Live-Postgres tests for ADR 0055 P2's `mount_catalog`: the upsert-by-name
//! `register_skill`, `list_skills` / `get_skill_by_name`, `soft_delete_skill`,
//! and — the load-bearing seam — the catalog fold into `bundle_pin_set`
//! (snapshot-pins ∪ **live** catalog skills). The pin-set union is what carries
//! a registered skill into every host's `live_bundles` so it stages on-demand;
//! a soft-deleted skill must drop out so the bundle GC can reclaim its blob.
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`:
//!
//! ```bash
//! just db-up
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test mount_catalog_live_pg -- --ignored
//! ```

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use uuid::Uuid;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
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

/// Unique-per-run fake sha256 hex so concurrent CI runs don't trample.
fn fake_sha() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[tokio::test]
#[ignore = "requires ENGRAM_TEST_DATABASE_URL (Postgres)"]
async fn register_resolves_pins_upserts_and_soft_deletes() {
    let Some(meta) = connect().await else { return };
    let name = format!("e2e-skill-{}", Uuid::new_v4().simple());
    let sha = fake_sha();

    // Register.
    let row = meta
        .register_skill("owner-a", &name, "first", &sha, r#"{"kind":"skill"}"#, 123)
        .await
        .expect("register_skill");
    assert_eq!(row.name, name);
    assert_eq!(row.owner, "owner-a");
    assert_eq!(row.sha256, sha);
    assert_eq!(row.size_bytes, 123);

    // get_skill_by_name resolves it (the session-create resolver's catalog leg).
    let got = meta
        .get_skill_by_name(&name)
        .await
        .expect("get_skill_by_name")
        .expect("present");
    assert_eq!(got.id, row.id);
    assert_eq!(got.sha256, sha);

    // list_skills contains it.
    assert!(
        meta.list_skills().await.unwrap().iter().any(|s| s.name == name),
        "list_skills must include the registered skill",
    );

    // The catalog sha is in the pin set (∪ with snapshot pins) — so it reaches
    // every host's `live_bundles` and stages on-demand.
    assert!(
        meta.bundle_pin_set().await.unwrap().iter().any(|r| r.sha256 == sha),
        "bundle_pin_set must include a live catalog skill's sha",
    );

    // Upsert by name: re-register the same name with new content keeps the id +
    // swaps the sha (the resolve + pin set follow the new content).
    let sha2 = fake_sha();
    let row2 = meta
        .register_skill("owner-b", &name, "second", &sha2, "{}", 456)
        .await
        .expect("re-register");
    assert_eq!(row2.id, row.id, "upsert-by-name keeps the row id stable");
    assert_eq!(row2.owner, "owner-b");
    assert_eq!(row2.size_bytes, 456);
    assert_eq!(
        meta.get_skill_by_name(&name).await.unwrap().unwrap().sha256,
        sha2,
    );
    let pins = meta.bundle_pin_set().await.unwrap();
    assert!(pins.iter().any(|r| r.sha256 == sha2), "new sha pinned");
    assert!(
        !pins.iter().any(|r| r.sha256 == sha),
        "the superseded sha is no longer pinned by the catalog",
    );

    // Soft-delete → gone from resolve + list + pin set (upload-path GC enabled),
    // and idempotent.
    assert!(meta.soft_delete_skill(&name).await.unwrap(), "deleted a live row");
    assert!(meta.get_skill_by_name(&name).await.unwrap().is_none());
    assert!(!meta.list_skills().await.unwrap().iter().any(|s| s.name == name));
    assert!(
        !meta.bundle_pin_set().await.unwrap().iter().any(|r| r.sha256 == sha2),
        "a soft-deleted skill drops out of the pin set",
    );
    assert!(
        !meta.soft_delete_skill(&name).await.unwrap(),
        "deleting an absent skill is idempotent (false)",
    );

    // The freed name re-registers as a fresh row (the partial unique index only
    // covers live rows).
    let row3 = meta
        .register_skill("owner-c", &name, "third", &fake_sha(), "{}", 1)
        .await
        .expect("re-register after delete");
    assert_ne!(row3.id, row.id, "a fresh row after the soft-delete");

    // Cleanup.
    meta.soft_delete_skill(&name).await.unwrap();
}
