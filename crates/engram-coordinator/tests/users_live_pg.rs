//! Live-Postgres tests for the ADR 0031 `UserStore` + `WebSessionStore`
//! implementations: JIT upsert idempotence + role preservation, role/active
//! mutation, sealed-token round-trip, and web-session lookup honouring
//! expiry + the deprovisioning (inactive-user) gate.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test users_live_pg -- --ignored
//! ```

use chrono::{Duration, Utc};
use engram_core::traits::{UserStore, WebSessionStore};
use engram_core::types::user::{Role, RoleSource, UserToken, WebSession};
use engram_postgres::PostgresStore;

async fn connect() -> Option<PostgresStore> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: ENGRAM_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let store = PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(store)
}

/// A fresh, unique email per test so parallel runs / reruns don't collide on
/// the `users.email` UNIQUE constraint.
fn unique_email(tag: &str) -> String {
    format!("{tag}+{}@example.test", uuid::Uuid::new_v4())
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn jit_upsert_is_idempotent_and_preserves_role() {
    let Some(store) = connect().await else {
        return;
    };
    let email = unique_email("jit");

    // First login: created as member via a claim.
    let u1 = store
        .upsert_user_by_email(&email, Some("Ada"), Role::Member, RoleSource::Claim)
        .await
        .unwrap();
    assert_eq!(u1.email, email);
    assert_eq!(u1.role, Role::Member);
    assert_eq!(u1.display_name.as_deref(), Some("Ada"));
    assert!(u1.active);

    // An admin promotes them manually.
    let promoted = store
        .set_user_role(u1.id, Role::Admin, RoleSource::Manual)
        .await
        .unwrap();
    assert_eq!(promoted.role, Role::Admin);
    assert_eq!(promoted.role_source, RoleSource::Manual);

    // A returning login must NOT clobber the manual promotion, and must keep
    // the same row id.
    let u2 = store
        .upsert_user_by_email(&email, Some("Ada L."), Role::Member, RoleSource::Claim)
        .await
        .unwrap();
    assert_eq!(
        u2.id, u1.id,
        "upsert must be keyed on email, not insert anew"
    );
    assert_eq!(u2.role, Role::Admin, "manual role survives re-login");
    assert_eq!(
        u2.display_name.as_deref(),
        Some("Ada L."),
        "display name refreshed"
    );

    // Lookup by email + id agree.
    let by_email = store.get_user_by_email(&email).await.unwrap().unwrap();
    assert_eq!(by_email.id, u1.id);
    assert_eq!(store.get_user(u1.id).await.unwrap().id, u1.id);
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn sealed_token_round_trips_and_replaces() {
    let Some(store) = connect().await else {
        return;
    };
    let user = store
        .upsert_user_by_email(&unique_email("tok"), None, Role::Member, RoleSource::Claim)
        .await
        .unwrap();

    assert!(store
        .get_user_token(user.id, UserToken::KIND_CLAUDE_OAUTH)
        .await
        .unwrap()
        .is_none());

    let mk = |cipher: &[u8]| UserToken {
        user_id: user.id,
        kind: UserToken::KIND_CLAUDE_OAUTH.to_string(),
        wrapped_dek: vec![1, 2, 3],
        nonce: vec![4; 12],
        ciphertext: cipher.to_vec(),
        key_id: "test:v1".into(),
        created_at: Utc::now(),
        updated_at: None,
    };

    store.upsert_user_token(mk(b"sealed-A")).await.unwrap();
    let got = store
        .get_user_token(user.id, UserToken::KIND_CLAUDE_OAUTH)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.ciphertext, b"sealed-A");

    // Saving again replaces (one row per (user, kind)).
    store.upsert_user_token(mk(b"sealed-B")).await.unwrap();
    let got = store
        .get_user_token(user.id, UserToken::KIND_CLAUDE_OAUTH)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.ciphertext, b"sealed-B");

    store
        .delete_user_token(user.id, UserToken::KIND_CLAUDE_OAUTH)
        .await
        .unwrap();
    assert!(store
        .get_user_token(user.id, UserToken::KIND_CLAUDE_OAUTH)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn web_session_lookup_honours_expiry_and_active_gate() {
    let Some(store) = connect().await else {
        return;
    };
    let user = store
        .upsert_user_by_email(&unique_email("ws"), None, Role::Member, RoleSource::Claim)
        .await
        .unwrap();
    let now = Utc::now();

    let live = WebSession {
        token_hash: format!("live-{}", user.id).into_bytes(),
        user_id: user.id,
        created_at: now,
        expires_at: now + Duration::hours(1),
        last_seen_at: now,
    };
    let expired = WebSession {
        token_hash: format!("expired-{}", user.id).into_bytes(),
        user_id: user.id,
        created_at: now - Duration::hours(2),
        expires_at: now - Duration::hours(1),
        last_seen_at: now - Duration::hours(2),
    };
    store.create_web_session(live.clone()).await.unwrap();
    store.create_web_session(expired.clone()).await.unwrap();

    // Live cookie resolves to (session, user).
    let resolved = store.lookup_web_session(&live.token_hash).await.unwrap();
    let (_, u) = resolved.expect("live session resolves");
    assert_eq!(u.id, user.id);

    // Expired cookie does not resolve.
    assert!(store
        .lookup_web_session(&expired.token_hash)
        .await
        .unwrap()
        .is_none());

    // Deprovision the user → the live cookie stops resolving even though it
    // hasn't expired.
    store.set_user_active(user.id, false).await.unwrap();
    assert!(
        store
            .lookup_web_session(&live.token_hash)
            .await
            .unwrap()
            .is_none(),
        "inactive user must not ride a live cookie"
    );

    // Revoke-all clears the rows.
    store.set_user_active(user.id, true).await.unwrap();
    store.revoke_all_for_user(user.id).await.unwrap();
    assert!(store
        .lookup_web_session(&live.token_hash)
        .await
        .unwrap()
        .is_none());
}
