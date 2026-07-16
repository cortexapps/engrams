//! Live-Postgres integration test for Phase 3c HA.
//!
//! Spins up two `AppState`s sharing a real Postgres, attaches an SSE
//! subscriber to coord-A, and emits an event via coord-B's
//! `AppState::emit`. The `pg_notify('session_events', ...)` fired by
//! `append_session_event` reaches coord-A's `pg_listener` task, which
//! re-broadcasts the typed `SessionEvent` into coord-A's local
//! `SessionEventBus` so the subscriber sees it.
//!
//! `#[ignore]`'d by default — requires Postgres reachable at the URL
//! pointed to by `ENGRAM_TEST_DATABASE_URL` (the local
//! `deploy/docker-compose.dev.yml` brings one up at
//! `postgres://engram:engram@localhost:5435/engram`). To run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test ha_listener -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::MetadataStore;
use engram_core::types::SessionSpec;

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn cross_replica_event_fan_out() {
    // ADR 0099 H1: private template-cloned database — this binary's two
    // tests both LISTEN/NOTIFY, and NOTIFY is per-database, so a shared
    // database means cross-talk (the reason the CI PG lane used to run
    // `--test-threads=1`).
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let database_url = db.url.clone();
    let meta: Arc<dyn MetadataStore> = Arc::new(db.store);

    // Seed a session row both AppStates can refer to. The producer
    // (coord-B) appends an event against this id; the subscriber on
    // coord-A waits for it.
    let session_id = meta
        .create_session(SessionSpec {
            image: "ha-listener-test:warm-test".into(),
            mode: engram_core::types::session::SessionMode::Agent,
        })
        .await
        .expect("create session");

    let coord_a = build_app_state(meta.clone(), &database_url).await;
    let coord_b = build_app_state(meta.clone(), &database_url).await;

    // Subscribe BEFORE emitting so we don't race the broadcast.
    // The pg_listener task in each AppState was spawned by
    // `run_with_registry`; for this test we instead spawn it
    // explicitly against the shared meta+events.
    let mut rx = coord_a.events.subscribe(session_id);

    // Producer-side: emit through coord-B. This persists + fires
    // pg_notify; coord-A's listener picks it up and re-broadcasts.
    let test_chunk = format!("ha-test-{}", uuid::Uuid::new_v4().simple());
    let event = engram_coordinator::state::SessionEvent::Stdout {
        exec_id: "test-exec".into(),
        chunk: test_chunk.clone(),
    };

    // coord-A's pg_listener may still be establishing its `LISTEN` when we
    // emit — and Postgres only delivers a `NOTIFY` to channels listening AT
    // notify time, so a single emit can race the listener startup and be
    // silently lost (the old fixed 150ms sleep flaked on slow CI runners).
    // Re-emit on a short interval until coord-A receives the chunk, or a
    // generous deadline. Re-emitting is safe: duplicate same-chunk events
    // just queue on `rx`, and we assert on the first one we read.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let received = loop {
        coord_b
            .emit(session_id, event.clone())
            .await
            .expect("coord-b emit");
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(r) => break r.expect("subscriber wasn't dropped"),
            Err(_) => assert!(
                tokio::time::Instant::now() < deadline,
                "coord-A never received the cross-replica event within 10s — \
                 LISTEN/NOTIFY fan-out is broken (not just slow)",
            ),
        }
    };

    match received.event {
        engram_coordinator::state::SessionEvent::Stdout { chunk, .. } => {
            assert_eq!(
                chunk, test_chunk,
                "coord-A must see the chunk emitted by coord-B byte-for-byte"
            );
        }
        other => panic!(
            "expected Stdout cross-replica event, got {other:?} (the bridge \
             is decoding the wrong variant or fan-out went sideways)"
        ),
    }
}

async fn build_app_state(
    meta: Arc<dyn MetadataStore>,
    database_url: &str,
) -> Arc<engram_coordinator::AppState> {
    use engram_cloud_mock::MockCloud;
    use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};

    let work_dir = tempfile::tempdir().expect("work dir").keep();

    let raw: Arc<dyn engram_core::traits::SandboxBackend> =
        Arc::new(engram_sandbox_process::ProcessBackend::new(work_dir));
    let pooled: Arc<dyn engram_core::traits::SandboxBackend> =
        Arc::new(engram_host_agent::pooled_backend::PooledBackend::new(raw));

    let services = Services {
        meta: meta.clone(),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(pooled)),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
        blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        )),
        chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
            engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            ),
        )),
        host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
    };
    let cfg = CoordinatorConfig {
        database_url: database_url.to_string(),
        ..CoordinatorConfig::default()
    };
    let registry = Arc::new(HostRegistry::new(meta.clone()));
    registry.register(engram_core::HostId::new(), services.host.clone());
    let state = Arc::new(AppState::new_with_registry(cfg, services, registry));

    // Spawn a pg_listener bound to this AppState's event bus.
    // `run_with_registry` does this in production; for the test we
    // wire it up directly so we don't need to bind an axum server.
    // The handle is intentionally dropped — the task lives for the
    // test's duration and the runtime collects it on shutdown.
    drop(engram_coordinator::pg_listener::spawn(
        database_url.to_string(),
        meta,
        state.events.clone(),
        state.host_registry.clone(),
        state.integrations.clone(),
        state.boot_bundles.clone(),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    ));
    state
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn append_session_event_fires_pg_notify() {
    // Targeted check that the SQL change in `append_session_event`
    // actually emits a NOTIFY (and not, say, a silent INSERT).
    // Subscribes a raw PgListener and counts notifications.
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let database_url = db.url;
    let store = db.store;

    let mut listener = sqlx::postgres::PgListener::connect(&database_url)
        .await
        .expect("listener connect");
    listener
        .listen("session_events")
        .await
        .expect("listen session_events");

    let session_id = store
        .create_session(SessionSpec {
            image: "ha-notify-test:warm-test".into(),
            mode: engram_core::types::session::SessionMode::Agent,
        })
        .await
        .expect("create");

    // Give the listener time to settle on the channel.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let _idx = store
        .append_session_event(
            session_id,
            "stdout",
            serde_json::json!({"type": "stdout", "exec_id": "x", "chunk": "hi"}),
        )
        .await
        .expect("append");

    let notification = tokio::time::timeout(Duration::from_secs(2), listener.recv())
        .await
        .expect("notification arrived")
        .expect("listener stayed open");

    assert_eq!(notification.channel(), "session_events");
    let payload: serde_json::Value =
        serde_json::from_str(notification.payload()).expect("payload is JSON");
    assert_eq!(
        payload["session_id"].as_str(),
        Some(session_id.to_string().as_str()),
        "NOTIFY payload should carry the session id we just appended for"
    );
    assert!(payload["idx"].is_number(), "payload includes idx");

    // Smoke read of the row through the public API to confirm the
    // INSERT committed alongside the NOTIFY (the CTE wraps both in
    // one statement; this catches a regression that decouples them).
    let row = store
        .list_active_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == session_id)
        .expect("session row is present after the NOTIFY arrived");
    assert_eq!(row.id, session_id);
}

/// ADR 0047: the stateless-coordinator contract, end to end against one
/// shared Postgres — what one replica writes (heartbeat scheduling
/// state, a durable cordon, a teleport pin, a sealed broker token), any
/// other replica reads on its next decision. Two `PostgresStore`s stand
/// in for two coord pods; a unique image digest isolates the candidate
/// pool from any other host rows in the shared test database.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn cross_replica_scheduling_pins_and_tokens() {
    use engram_coordinator::host_registry::HostRegistry;
    use engram_coordinator::placement::{self, ScheduleContext};
    use engram_core::types::host::{HostCapacity, HostHeartbeat, HostRecord, HostStatus};
    use engram_core::HostId;

    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let store_b = engram_postgres::PostgresStore::connect(&db.url)
        .await
        .expect("connect B");
    let meta_a: Arc<dyn MetadataStore> = Arc::new(db.store);
    let meta_b: Arc<dyn MetadataStore> = Arc::new(store_b);

    // --- 1. heartbeat through A ⇒ schedulable from B -----------------
    let digest = format!("sha256:adr47-{}", uuid::Uuid::new_v4().simple());
    let h1 = HostId::new();
    let h2 = HostId::new();
    for (host, name) in [(h1, "ha-h1"), (h2, "ha-h2")] {
        meta_a
            .upsert_host(HostRecord {
                id: host,
                hostname: name.into(),
                cloud_metadata: Default::default(),
                capacity: HostCapacity {
                    total_gb: 0,
                    used_gb: 0,
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: None,
                ready_images: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: false,
                capabilities: engram_core::types::host::HostCapabilities::default(),
            })
            .await
            .expect("seed host");
        meta_a
            .touch_host_heartbeat(
                host,
                HostHeartbeat {
                    status: HostStatus::Ready,
                    capacity: HostCapacity {
                        total_gb: 0,
                        used_gb: 0,
                        total_mib: 16_384,
                        used_mib: 0,
                        running_sandboxes: 0,
                    },
                    utilization: Default::default(),
                    ready_images: vec![digest.clone()],
                    current_bundles: Vec::new(),
                    total_vcpus: 8,
                    wire_version: engram_protocol::WIRE_VERSION,
                    stages_images: false,
                    capabilities: engram_core::types::host::HostCapabilities::default(),
                },
            )
            .await
            .expect("heartbeat through A");
    }
    // Replica B has its own registry with its own backends — the
    // scheduling DECISION comes from the shared rows.
    let registry_b = Arc::new(HostRegistry::new(meta_b.clone()));
    let work = tempfile::tempdir().expect("dir").keep();
    let raw: Arc<dyn engram_core::traits::SandboxBackend> =
        Arc::new(engram_sandbox_process::ProcessBackend::new(work));
    let backend: Arc<dyn engram_core::traits::HostClient> =
        Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(raw));
    registry_b.register(h1, backend.clone());
    registry_b.register(h2, backend.clone());

    let ctx = ScheduleContext {
        repo: "r",
        image_version: "v",
        snapshot_host: None,
        memory_mib: None,
        cpu_budget_vcpus: None,
        required_image_digest: Some(engram_protocol::heartbeat::ManifestDigest::new(
            digest.clone(),
        )),
        exclude_host: None,
        prefer_host: None,
        caps: Default::default(),
        prefer_bundles: &[],
    };
    let (picked, _) = placement::pick_for_session(meta_b.as_ref(), &registry_b, &ctx)
        .await
        .expect("B schedules onto a host whose heartbeats landed on A");
    assert!(picked == h1 || picked == h2);

    // --- 2. cordon via A ⇒ B's picker excludes it ---------------------
    meta_a.set_host_cordoned(h1, true).await.expect("cordon");
    for _ in 0..10 {
        let (picked, _) = placement::pick_for_session(meta_b.as_ref(), &registry_b, &ctx)
            .await
            .expect("pick");
        assert_eq!(picked, h2, "A's cordon must bind B's picker");
    }

    // --- 3. teleport pin via A ⇒ visible (and clearable) from B ------
    let session_id = meta_a
        .create_session(SessionSpec {
            image: "ha-adr47:test".into(),
            mode: engram_core::types::session::SessionMode::Agent,
        })
        .await
        .expect("create session");
    meta_a
        .set_teleport_target(session_id, Some(h2))
        .await
        .expect("pin via A");
    assert_eq!(
        meta_b
            .get_teleport_target(session_id)
            .await
            .expect("get via B")
            .map(|(h, _set_at)| h),
        Some(h2),
        "B's scanner must honor A's pin"
    );
    meta_b
        .set_teleport_target(session_id, None)
        .await
        .expect("clear via B");
    assert_eq!(
        meta_a.get_teleport_target(session_id).await.expect("get"),
        None
    );

    // --- 4. broker token sealed via A ⇒ unsealed + equal on B --------
    let kek_a = engram_crypto::EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1");
    let kek_b = engram_crypto::EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1");
    let token = format!("tok-{}", uuid::Uuid::new_v4().simple());
    let sealed = engram_crypto::CredCipher::new(&kek_a)
        .seal(token.as_bytes())
        .await
        .expect("seal");
    let inserted = meta_a
        .insert_broker_token(engram_core::types::registry::SessionBrokerToken {
            session_id,
            wrapped_dek: sealed.wrapped_dek.clone(),
            nonce: sealed.nonce.to_vec(),
            ciphertext: sealed.ciphertext.clone(),
            key_id: sealed.key_id.clone(),
        })
        .await
        .expect("insert");
    assert!(inserted, "first writer wins");
    // A losing replica's re-insert is a clean no-op…
    let second = meta_b
        .insert_broker_token(engram_core::types::registry::SessionBrokerToken {
            session_id,
            wrapped_dek: vec![1],
            nonce: vec![0; 12],
            ciphertext: vec![2],
            key_id: "loser".into(),
        })
        .await
        .expect("racing insert");
    assert!(!second, "ON CONFLICT DO NOTHING — the loser re-reads");
    // …and B unseals the winner's token to the same plaintext.
    let row = meta_b
        .get_broker_token(session_id)
        .await
        .expect("get via B")
        .expect("row exists");
    let nonce: [u8; 12] = row.nonce.as_slice().try_into().expect("12-byte nonce");
    let opened = engram_crypto::CredCipher::new(&kek_b)
        .open(&engram_crypto::SealedCred {
            wrapped_dek: row.wrapped_dek,
            nonce,
            ciphertext: row.ciphertext,
            key_id: row.key_id,
        })
        .await
        .expect("open via B");
    assert_eq!(String::from_utf8(opened).unwrap(), token);
    meta_b
        .delete_broker_token(session_id)
        .await
        .expect("delete");
    assert!(meta_a
        .get_broker_token(session_id)
        .await
        .expect("get after delete")
        .is_none());
}

/// Regression (ADR 0051 git-credential injection): the per-session
/// git/upload broker token (ADR 0047, PG-backed) FKs to `sessions.id`, so it
/// CANNOT be minted before the session row exists. The gRPC create path used
/// to mint it inside `prepare_inner` while building the AgentSpec — BEFORE
/// `create_session_created` committed the row — so the insert silently
/// FK-failed (`get_or_mint_broker_token` swallows the error to `None`) and the
/// guest got no `ENGRAM_FORGE_TOKEN`: git failed with "could not read
/// Username" (prod session a8395112). The in-memory mock store in
/// `grpc_app.rs` enforces no FK and so can't catch this — it's pinned here
/// against real Postgres. The fix defers `inject_harness_env` to
/// `boot_on_reserved_host`, after the row materializes.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn broker_token_insert_requires_session_row() {
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let meta: Arc<dyn MetadataStore> = Arc::new(db.store);

    // A broker token for a session whose row doesn't exist yet.
    let orphan = engram_core::types::SessionId::new();
    let tok = engram_core::types::registry::SessionBrokerToken {
        session_id: orphan,
        wrapped_dek: vec![1],
        nonce: vec![0; 12],
        ciphertext: vec![2],
        key_id: "k".into(),
    };

    // No session row → the FK rejects the insert. This is the error the
    // coordinator USED to swallow into a silent no-op; the store must surface
    // it (not return Ok) so the caller can't mistake "FK failed" for "minted".
    let res = meta.insert_broker_token(tok.clone()).await;
    assert!(
        res.is_err(),
        "broker token insert must FK-fail without a session row, got {res:?}",
    );
    assert!(
        meta.get_broker_token(orphan).await.expect("get").is_none(),
        "a FK-rejected insert must leave no row",
    );

    // Once the session row exists (the order `boot_on_reserved_host` now
    // guarantees), the same insert succeeds and round-trips.
    let session_id = meta
        .create_session(SessionSpec {
            image: "ha-listener-test:broker-fk".into(),
            mode: engram_core::types::session::SessionMode::Agent,
        })
        .await
        .expect("create session");
    let inserted = meta
        .insert_broker_token(engram_core::types::registry::SessionBrokerToken { session_id, ..tok })
        .await
        .expect("insert after the session row exists");
    assert!(inserted, "first writer wins once the FK target exists");
    assert!(
        meta.get_broker_token(session_id)
            .await
            .expect("get")
            .is_some(),
        "broker token persists once minted after the session row",
    );
    meta.delete_broker_token(session_id).await.expect("cleanup");
}
