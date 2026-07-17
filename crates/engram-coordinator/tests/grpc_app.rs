//! In-process integration tests for the app-gRPC surface (ADR 0051).
//! The coordinator's web-facing REST surface is gone; the
//! orchestrator drives the coordinator exclusively over the four tonic
//! services in `grpc_app/` (SessionService, ShellRelayService,
//! FleetService, ImageService). This file is the gRPC replacement for the
//! REST coverage that `tests/api.rs` used to carry over `tower::oneshot`
//! against `api::router`.
//!
//! It serves the real `grpc_app::server` router on an ephemeral
//! 127.0.0.1 port and drives real tonic clients through it, exercising:
//!   - bearer auth: valid / missing / wrong → Unauthenticated; a server
//!     configured with ZERO tokens fails closed (the deliberate opposite
//!     of the old axum empty-list dev bypass);
//!   - SessionService create → get → list → delete round-trips;
//!   - bad-UUID → InvalidArgument, not-found → NotFound error mapping;
//!   - FleetService list_hosts + get_storage_summary;
//!   - ImageService list_enabled_images.
//!
//! The metadata store is `engram_sim::SimMetadataStore` (ADR 0098 D4) —
//! the conformance-tested in-memory `MetadataStore` — so create / get /
//! list / delete round-trips (and the op-log-driven DeleteSession) are
//! exercised against the same observable semantics PostgresStore has.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use engram_cloud_mock::MockCloud;
use engram_coordinator::{grpc_app, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::outbox::OutboxRow;
use engram_core::types::{SessionSpec, SessionState};
use engram_core::SessionId;
use engram_protocol::app;
use engram_protocol::app::session_service_server::SessionService as _;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{SimEntropy, SimMetadataStore};

// ADR 0098 D4: the hand-rolled `MockMetadataStore` (a self-contained copy
// of tests/api.rs's) is retired onto the conformance-tested
// `SimMetadataStore`. Sessions are staged through REAL store calls; the
// event log and outbox are read back through the store's own state
// (helpers below) rather than a mock's raw HashMaps.

fn sim_meta() -> Arc<SimMetadataStore> {
    SimMetadataStore::new(
        Arc::new(engram_core::traits::SystemClock::new()),
        Arc::new(SimEntropy::seeded(0x6A9C)),
    )
}

/// Every persisted event for `sid` as `(kind, payload)` — the faithful
/// store keeps them in its event log like PG does (the retired mock
/// exposed a raw HashMap).
fn session_events(meta: &SimMetadataStore, sid: SessionId) -> Vec<(String, serde_json::Value)> {
    meta.with_db(|db| {
        db.session_events
            .get(&sid)
            .into_iter()
            .flatten()
            .map(|e| (e.kind.clone(), e.payload.clone()))
            .collect()
    })
}

/// The outbox keyed by prompt_id (the store's own table).
fn outbox_rows(meta: &SimMetadataStore) -> std::collections::BTreeMap<String, OutboxRow> {
    meta.with_db(|db| db.outbox.clone())
}

// ---------------------------------------------------------------------
// Fixture: a fully-wired AppState over in-memory components, mirroring
// tests/api.rs::build_app_with_tokens but with the app-gRPC bearer
// allow-list (`app_grpc_tokens`) set instead of the REST `auth_tokens`.
// `app_grpc_tokens` empty == fail closed.
// ---------------------------------------------------------------------

/// Token the test server is configured with — the happy-path credential.
const TEST_TOKEN: &str = "test-app-grpc-token";

fn test_state(app_grpc_tokens: Vec<String>) -> (Arc<AppState>, Arc<SimMetadataStore>) {
    let meta = sim_meta();
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let blob = || {
        Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        ))
    };
    let services = Services {
        meta: meta.clone(),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
        secrets: Arc::new(InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob: blob(),
        chunk_store: engram_chunk_store::ChunkStore::new(blob()),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
        clock: Arc::new(engram_core::traits::SystemClock::new()),
        entropy: Arc::new(engram_core::traits::OsEntropy),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        app_grpc_tokens,
        ..CoordinatorConfig::default()
    };
    (Arc::new(AppState::new(cfg, services)), meta)
}

/// Bring up the app-gRPC server on an ephemeral port; returns the dial
/// address and the join handle so the caller can `abort()` it.
async fn serve(state: Arc<AppState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming from listener");
    let handle = tokio::spawn(async move {
        let _ = grpc_app::server(state).serve_with_incoming(incoming).await;
    });
    (addr, handle)
}

/// Dial the app-gRPC server at `addr` and return a connected channel.
async fn dial(addr: std::net::SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint uri")
        .connect()
        .await
        .expect("dial app gRPC")
}

/// tonic interceptor that stamps `Authorization: Bearer <token>` onto
/// every outbound request.
#[allow(clippy::result_large_err)]
fn bearer(
    token: &'static str,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("ascii header"),
        );
        Ok(req)
    }
}

/// Minimal `EnabledImage` row for the ImageService list + update tests
/// (ADR 0080: the summary's `config` mirrors the row's image_config).
/// `suggested_vcpus` is set because `ImageConfig::validate` requires it
/// (ADR 0048), so an UpdateImage that round-trips this config validates
/// AND diffs clean against the row (no phantom `resources` change).
fn enabled_image(uri: &str) -> engram_core::types::EnabledImage {
    let now = Utc::now();
    engram_core::types::EnabledImage {
        id: uuid::Uuid::new_v4(),
        image_uri: uri.to_string(),
        image_config: engram_core::types::image::ImageConfig {
            name: "test".into(),
            resources: engram_core::types::image::ResourceHints {
                suggested_vcpus: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
        oci_defaults: Default::default(),
        manifest_digest: "sha256:0000".to_string(),
        disk_manifest: None,
        base_snapshot_id: None,
        base_snapshot_disk_manifest: None,
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        soft_deleted_at: None,
    }
}

// =====================================================================
// Auth (ADR 0051 §5) — replaces tests/api.rs auth_* REST middleware tests
// =====================================================================

/// Happy path: a server configured with `TEST_TOKEN`, called with a
/// matching bearer, passes auth on every service. Proves each of the
/// four services is mounted and answering against the in-memory state.
#[tokio::test]
async fn app_grpc_valid_bearer_reaches_every_service() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;

    // SessionService.ListSessions — empty against fresh state.
    let resp = app::session_service_client::SessionServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect("ListSessions must succeed with a valid bearer");
    assert!(
        resp.into_inner().sessions.is_empty(),
        "fresh state has no sessions"
    );

    // FleetService.ListHosts — empty.
    let resp = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_hosts(app::ListHostsRequest::default())
    .await
    .expect("FleetService ListHosts must succeed");
    assert!(resp.into_inner().hosts.is_empty(), "no hosts");

    // ImageService.ListEnabledImages — empty.
    let resp = app::image_service_client::ImageServiceClient::with_interceptor(
        channel.clone(),
        bearer(TEST_TOKEN),
    )
    .list_enabled_images(app::ListEnabledImagesRequest::default())
    .await
    .expect("ImageService ListEnabledImages must succeed");
    assert!(resp.into_inner().images.is_empty(), "no images");

    // ShellRelayService.Relay — an empty inbound stream returns
    // InvalidArgument (no open frame), proving the service is mounted and
    // the auth check passed (we'd see Unauthenticated otherwise).
    let outbound = tokio_stream::iter(Vec::<app::RelayShellRequest>::new());
    let err = app::shell_relay_service_client::ShellRelayServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .relay(outbound)
    .await
    .expect_err("Relay: empty stream must return InvalidArgument (no open frame)");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    server.abort();
}

/// On a token-configured server, a call with no bearer and a call with
/// the wrong bearer are both rejected `Unauthenticated`. Replaces the
/// REST `auth_rejects_*` middleware tests.
#[tokio::test]
async fn app_grpc_rejects_missing_and_wrong_bearer() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;

    // No Authorization metadata at all.
    let err = app::session_service_client::SessionServiceClient::new(channel.clone())
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect_err("missing bearer must be refused");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    // A bearer that isn't in the allow-list.
    let err = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer("not-the-token"),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect_err("wrong bearer must be refused");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    server.abort();
}

/// Fail-closed: a server built with ZERO configured tokens rejects
/// everything — even a syntactically valid bearer. The deliberate
/// opposite of the retired axum surface's empty-list dev bypass.
#[tokio::test]
async fn app_grpc_fails_closed_with_no_tokens_configured() {
    let (state, _meta) = test_state(vec![]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;

    let err = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    )
    .list_sessions(app::ListSessionsRequest::default())
    .await
    .expect_err("fail-closed: no tokens means reject everything");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");

    server.abort();
}

// =====================================================================
// SessionService CRUD — replaces the REST get/list/delete + error-mapping
// tests in tests/api.rs (the create path needs a live sandbox/host and is
// covered by the gated grpc_smoke.rs against a real stack).
// =====================================================================

/// Seed two sessions directly via the MetadataStore (the create RPC needs
/// a live sandbox), then drive GetSession / ListSessions / DeleteSession
/// over gRPC and assert the round-trips. Replaces REST
/// `get_session_*` / `list_sessions_*` / `delete_session_*`.
#[tokio::test]
async fn session_get_list_delete_round_trip() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);

    // Seed a live (Active) session and a terminal one so ListSessions
    // (live-only) filtering is exercised.
    let live_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed live session");
    // Drive it to Active so list_active_sessions returns it.
    for target in [SessionState::Created, SessionState::Active] {
        meta.transition_session(live_id, target)
            .await
            .expect("transition to active");
    }

    let dead_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed dead session");
    for target in [
        SessionState::Created,
        SessionState::Active,
        SessionState::Completed,
    ] {
        meta.transition_session(dead_id, target)
            .await
            .expect("transition to completed");
    }

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // ---- GetSession on the live id ----
    let got = client
        .get_session(app::GetSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("GetSession must succeed")
        .into_inner()
        .session
        .expect("session must be present");
    assert_eq!(got.id, live_id.to_string(), "GetSession id mismatch");
    assert_eq!(got.image, "localhost:5001/demo:warm", "image mismatch");

    // ---- ListSessions returns only the live row ----
    let list = client
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions must succeed")
        .into_inner();
    let ids: Vec<String> = list
        .sessions
        .iter()
        .filter_map(|item| item.session.as_ref().map(|s| s.id.clone()))
        .collect();
    assert!(
        ids.contains(&live_id.to_string()),
        "live session must be listed; got {ids:?}"
    );
    assert!(
        !ids.contains(&dead_id.to_string()),
        "completed session must NOT be in the live list; got {ids:?}"
    );

    // ---- DeleteSession transitions to terminal (row persists) ----
    client
        .delete_session(app::DeleteSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("DeleteSession must succeed");

    // The row still exists, now terminal, and no longer in the live list.
    let post = client
        .get_session(app::GetSessionRequest {
            session_id: live_id.to_string(),
        })
        .await
        .expect("GetSession after delete must still return the row")
        .into_inner()
        .session
        .expect("row persists after delete");
    assert!(
        matches!(
            post.status.as_str(),
            "completed" | "failed" | "dead" | "host_lost"
        ),
        "deleted session must be terminal, got {:?}",
        post.status
    );

    server.abort();
}

fn complete_tool_call_service(state: Arc<AppState>) -> grpc_app::AppSessionService {
    grpc_app::AppSessionService {
        state,
        auth: Arc::new(grpc_app::auth::BearerAuth::new(vec![TEST_TOKEN.into()])),
    }
}

fn complete_tool_call_request(
    message: app::CompleteToolCallRequest,
) -> tonic::Request<app::CompleteToolCallRequest> {
    let mut req = tonic::Request::new(message);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {TEST_TOKEN}")
            .parse()
            .expect("ascii header"),
    );
    req
}

#[tokio::test]
async fn complete_tool_call_appends_submitted_event_and_enqueues_outbox() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    let session_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    let service = complete_tool_call_service(state);

    service
        .complete_tool_call(complete_tool_call_request(app::CompleteToolCallRequest {
            session_id: session_id.to_string(),
            tool_call_id: "call_1".into(),
            result_json: r#" { "saved": true } "#.into(),
        }))
        .await
        .expect("CompleteToolCall must succeed");

    let events = session_events(&meta, session_id);
    let submitted = events
        .iter()
        .find(|(kind, _)| kind == "tool_result_submitted")
        .expect("tool_result_submitted event");
    assert_eq!(submitted.1["tool_call_id"], "call_1");
    assert_eq!(submitted.1["result_json"], r#" { "saved": true } "#);

    let outbox = outbox_rows(&meta);
    let outbox_id = engram_core::types::outbox::tool_result_outbox_id(session_id, "call_1");
    let row = outbox.get(&outbox_id).expect("tool result outbox row");
    assert_eq!(row.kind, engram_core::types::outbox::OutboxKind::ToolResult);
    assert_eq!(row.payload["tool_call_id"], "call_1");
    assert_eq!(row.payload["result_json"], r#" { "saved": true } "#);
}

#[tokio::test]
async fn complete_tool_call_rejects_empty_tool_call_id() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    let session_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    let service = complete_tool_call_service(state);

    let err = service
        .complete_tool_call(complete_tool_call_request(app::CompleteToolCallRequest {
            session_id: session_id.to_string(),
            tool_call_id: String::new(),
            result_json: "{}".into(),
        }))
        .await
        .expect_err("empty tool_call_id must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    assert!(outbox_rows(&meta).is_empty());
}

#[tokio::test]
async fn complete_tool_call_rejects_terminal_session() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    let session_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    for target in [
        SessionState::Created,
        SessionState::Active,
        SessionState::Completed,
    ] {
        meta.transition_session(session_id, target)
            .await
            .expect("transition session");
    }
    let service = complete_tool_call_service(state);

    let err = service
        .complete_tool_call(complete_tool_call_request(app::CompleteToolCallRequest {
            session_id: session_id.to_string(),
            tool_call_id: "call_1".into(),
            result_json: "{}".into(),
        }))
        .await
        .expect_err("terminal session must be rejected");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    assert!(outbox_rows(&meta).is_empty());
}

#[tokio::test]
async fn complete_tool_call_is_idempotent_on_tool_call_id() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    let session_id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    let service = complete_tool_call_service(state);

    // A genuine retry resends IDENTICAL bytes; the tool_call_id-derived
    // prompt_id dedupes it to one outbox row. (The retired mock accepted a
    // same-id retry carrying a DIFFERENT payload and let the first win; the
    // faithful store — matching PostgresStore's ON CONFLICT + payload
    // compare — rejects that as a Conflict, so this scenario resends the
    // same payload, which is what a real retry does.)
    for _ in 0..2 {
        service
            .complete_tool_call(complete_tool_call_request(app::CompleteToolCallRequest {
                session_id: session_id.to_string(),
                tool_call_id: "call_1".into(),
                result_json: r#"{"saved":true}"#.into(),
            }))
            .await
            .expect("identical CompleteToolCall retry must succeed");
    }

    let outbox = outbox_rows(&meta);
    assert_eq!(outbox.len(), 1, "prompt_id uniqueness must dedupe retries");
    let outbox_id = engram_core::types::outbox::tool_result_outbox_id(session_id, "call_1");
    assert_eq!(
        outbox[&outbox_id].payload["result_json"], r#"{"saved":true}"#,
        "the deduped row carries the submitted result"
    );
}

#[tokio::test]
async fn complete_tool_call_namespaces_outbox_identity_by_session() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    let first = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed first session");
    let second = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".into(),
            mode: Default::default(),
        })
        .await
        .expect("seed second session");
    let service = complete_tool_call_service(state);

    for session_id in [first, second] {
        service
            .complete_tool_call(complete_tool_call_request(app::CompleteToolCallRequest {
                session_id: session_id.to_string(),
                tool_call_id: "shared-call".into(),
                result_json: r#"{"saved":true}"#.into(),
            }))
            .await
            .expect("same call id in another session must succeed");
    }

    let outbox = outbox_rows(&meta);
    assert!(
        outbox.contains_key(&engram_core::types::outbox::tool_result_outbox_id(
            first,
            "shared-call"
        ))
    );
    assert!(
        outbox.contains_key(&engram_core::types::outbox::tool_result_outbox_id(
            second,
            "shared-call"
        ))
    );
}

/// GetSession with a malformed (non-UUID) session id maps to
/// InvalidArgument; an unknown-but-valid UUID maps to NotFound. Replaces
/// REST `get_session_returns_400_for_malformed_id` /
/// `get_session_returns_404_for_unknown_id`.
#[tokio::test]
async fn session_get_bad_uuid_and_not_found_mapping() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let err = client
        .get_session(app::GetSessionRequest {
            session_id: "not-a-uuid".to_string(),
        })
        .await
        .expect_err("malformed id must error");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "malformed session id → InvalidArgument: {err:?}"
    );

    let unknown = SessionId::new();
    let err = client
        .get_session(app::GetSessionRequest {
            session_id: unknown.to_string(),
        })
        .await
        .expect_err("unknown id must error");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "unknown session id → NotFound: {err:?}"
    );

    server.abort();
}

/// CreateSession against an image that is not enabled returns
/// InvalidArgument (the image gate runs before any sandbox work).
/// Replaces REST `create_session_with_unknown_image_returns_400`.
#[tokio::test]
async fn create_session_unknown_image_is_invalid_argument() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let err = client
        .create_session(app::CreateSessionRequest {
            capabilities: Vec::new(),
            integration_policy_json: String::new(),
            selected_skills: Vec::new(),
            image_uri: "localhost:5001/never-enabled:warm".into(),
            mode: "agent".into(),
            prompt: None,
            harness_env: HashMap::new(),
            secrets: HashMap::new(),
            prompt_id: None,
            // ADR 0062: unused — this create fails at the unknown-image lookup
            // before harness resolution.
            harness: None,
        })
        .await
        .expect_err("non-enabled image must error");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "non-enabled image → InvalidArgument: {err:?}"
    );

    server.abort();
}

// =====================================================================
// FleetService — replaces REST storage_summary + (implicit) hosts list.
// =====================================================================

/// FleetService.ListHosts (empty) and GetStorageSummary (zeros) on a
/// fresh fleet. Replaces REST `storage_summary_zeros_on_empty_fleet`.
#[tokio::test]
async fn fleet_list_hosts_and_storage_summary() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::fleet_service_client::FleetServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let hosts = client
        .list_hosts(app::ListHostsRequest::default())
        .await
        .expect("ListHosts must succeed")
        .into_inner();
    assert!(hosts.hosts.is_empty(), "fresh fleet: no hosts");

    let summary = client
        .get_storage_summary(app::GetStorageSummaryRequest::default())
        .await
        .expect("GetStorageSummary must succeed")
        .into_inner();
    assert_eq!(summary.tracked_sandboxes, 0, "empty fleet: no sandboxes");
    assert_eq!(summary.snapshots, 0, "empty fleet: no snapshots");
    assert_eq!(summary.dirty_chunks, 0, "empty fleet: no dirty chunks");

    server.abort();
}

// =====================================================================
// ImageService — replaces REST enabled-images list.
// =====================================================================

/// ImageService.ListEnabledImages reflects an upserted enabled image.
/// Replaces REST list-enabled-images coverage.
#[tokio::test]
async fn image_list_enabled_images_reflects_store() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);

    // Empty to start.
    {
        let (addr, server) = serve(state.clone()).await;
        let channel = dial(addr).await;
        let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
            channel,
            bearer(TEST_TOKEN),
        );
        let resp = client
            .list_enabled_images(app::ListEnabledImagesRequest::default())
            .await
            .expect("ListEnabledImages must succeed")
            .into_inner();
        assert!(resp.images.is_empty(), "no images enabled yet");
        server.abort();
    }

    // Seed an enabled image and assert it surfaces over gRPC.
    meta.upsert_enabled_image(enabled_image("localhost:5001/demo:warm"))
        .await
        .expect("seed enabled image");

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );
    let resp = client
        .list_enabled_images(app::ListEnabledImagesRequest::default())
        .await
        .expect("ListEnabledImages must succeed")
        .into_inner();
    let uris: Vec<&str> = resp.images.iter().map(|i| i.image_uri.as_str()).collect();
    assert!(
        uris.contains(&"localhost:5001/demo:warm"),
        "enabled image must be listed; got {uris:?}"
    );

    server.abort();
}

// =====================================================================
// ImageService.UpdateImage (ADR 0080 phase 2b) — the mutability split:
// cheap fields (name/description/env/workdir) apply in place with NO
// job; resources/warm diffs are gated behind allow_recapture. The
// allow_recapture=true arm enqueues a real enable job (registry pull),
// so it's covered by the e2e stack, not here.
// =====================================================================

const UPDATE_URI: &str = "localhost:5001/demo:warm";

/// The proto twin of `enabled_image(...)`'s stored config — round-trips
/// clean (validates, and diffs empty against the seeded row). Tests
/// tweak fields off this base.
fn update_base_config() -> app::ImageConfig {
    app::ImageConfig {
        name: "test".into(),
        description: None,
        env: HashMap::new(),
        workdir: None,
        resources: Some(app::ImageResources {
            suggested_memory_mib: None,
            suggested_vcpus: Some(1),
            suggested_disk_gib: None,
        }),
        warm: None,
    }
}

fn update_request(config: app::ImageConfig, allow_recapture: bool) -> app::UpdateImageRequest {
    app::UpdateImageRequest {
        image_uri: UPDATE_URI.to_string(),
        config: Some(config),
        allow_recapture,
    }
}

/// Cheap edit (name/description only): succeeds WITHOUT allow_recapture,
/// returns NO job, and the store's row config is replaced in place.
#[tokio::test]
async fn image_update_cheap_edit_applies_in_place_without_job() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    meta.upsert_enabled_image(enabled_image(UPDATE_URI))
        .await
        .expect("seed enabled image");

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let edited = app::ImageConfig {
        name: "renamed".into(),
        description: Some("now with a description".into()),
        ..update_base_config()
    };
    let resp = client
        .update_image(update_request(edited, false))
        .await
        .expect("cheap edit must succeed without allow_recapture")
        .into_inner();
    assert!(
        resp.job.is_none(),
        "cheap edit must NOT enqueue a recapture job; got {resp:?}"
    );

    // The row's config was replaced in place (the cheap-edit write path).
    let row = meta
        .get_enabled_image(UPDATE_URI)
        .await
        .expect("get row")
        .expect("row still enabled");
    assert_eq!(row.image_config.name, "renamed", "name must be updated");
    assert_eq!(
        row.image_config.description.as_deref(),
        Some("now with a description"),
        "description must be updated"
    );
    assert_eq!(
        row.image_config.resources.suggested_vcpus,
        Some(1),
        "untouched resources must round-trip verbatim"
    );

    server.abort();
}

/// Capture-affecting edits (resources / warm) WITHOUT allow_recapture →
/// FailedPrecondition naming the offending field, and the row is left
/// untouched.
#[tokio::test]
async fn image_update_capture_affecting_diff_requires_allow_recapture() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    meta.upsert_enabled_image(enabled_image(UPDATE_URI))
        .await
        .expect("seed enabled image");

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // resources diff: bump suggested_vcpus 1 → 2.
    let resources_edit = app::ImageConfig {
        resources: Some(app::ImageResources {
            suggested_vcpus: Some(2),
            ..Default::default()
        }),
        ..update_base_config()
    };
    let err = client
        .update_image(update_request(resources_edit, false))
        .await
        .expect_err("resources diff without allow_recapture must be refused");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message().contains("resources"),
        "gate must name the `resources` field: {}",
        err.message()
    );

    // warm diff: add a [warm] hook.
    let warm_edit = app::ImageConfig {
        warm: Some(app::ImageWarmConfig {
            command: vec!["true".into()],
            timeout_secs: None,
            workdir: None,
            env: Vec::new(),
            network: None,
        }),
        ..update_base_config()
    };
    let err = client
        .update_image(update_request(warm_edit, false))
        .await
        .expect_err("warm diff without allow_recapture must be refused");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message().contains("warm"),
        "gate must name the `warm` field: {}",
        err.message()
    );

    // Neither refused edit touched the row.
    let row = meta
        .get_enabled_image(UPDATE_URI)
        .await
        .expect("get row")
        .expect("row still enabled");
    assert_eq!(
        row.image_config,
        enabled_image(UPDATE_URI).image_config,
        "a refused capture-affecting edit must leave the config untouched"
    );

    server.abort();
}

/// UpdateImage against a uri that isn't enabled → NotFound (editing a
/// disabled image is a re-enable's job, not an update's).
#[tokio::test]
async fn image_update_unknown_uri_is_not_found() {
    let (state, _meta) = test_state(vec![TEST_TOKEN.into()]);
    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    let err = client
        .update_image(update_request(update_base_config(), false))
        .await
        .expect_err("update of a never-enabled uri must error");
    assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");

    server.abort();
}

/// Structurally invalid configs → InvalidArgument before any store work:
/// missing config, empty name, and a missing `[resources] suggested_vcpus`
/// (required, ADR 0048).
#[tokio::test]
async fn image_update_invalid_config_is_invalid_argument() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);
    meta.upsert_enabled_image(enabled_image(UPDATE_URI))
        .await
        .expect("seed enabled image");

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::image_service_client::ImageServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // No config at all (the field is required — full replace).
    let err = client
        .update_image(app::UpdateImageRequest {
            image_uri: UPDATE_URI.to_string(),
            config: None,
            allow_recapture: false,
        })
        .await
        .expect_err("missing config must error");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // Empty name.
    let err = client
        .update_image(update_request(
            app::ImageConfig {
                name: "".into(),
                ..update_base_config()
            },
            false,
        ))
        .await
        .expect_err("empty name must error");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // Missing suggested_vcpus (ADR 0048: a declaration, not a hint).
    let err = client
        .update_image(update_request(
            app::ImageConfig {
                resources: None,
                ..update_base_config()
            },
            false,
        ))
        .await
        .expect_err("missing suggested_vcpus must error");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    server.abort();
}

// =====================================================================
// ListSessionEvents (ADR 0060 P1) — the unary bounded read of the event
// log that the SessionIngestWorkflow pump walks forward. Unfiltered:
// curation is the consumer's concern.
// =====================================================================

/// Page through five events: assert each batch's idxs + the returned
/// `next_after_idx` cursor, and that a read at the tail returns nothing and
/// echoes the cursor rather than rewinding.
#[tokio::test]
async fn session_list_events_paginates() {
    let (state, meta) = test_state(vec![TEST_TOKEN.into()]);

    let sid = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");

    // idx 0..=4 on the log.
    for kind in [
        "run_started",
        "user_question",
        "file_shared",
        "integration_asset",
        "run_completed",
    ] {
        meta.append_session_event(sid, kind, serde_json::json!({ "k": kind }))
            .await
            .expect("append event");
    }

    let (addr, server) = serve(state).await;
    let channel = dial(addr).await;
    let mut client = app::session_service_client::SessionServiceClient::with_interceptor(
        channel,
        bearer(TEST_TOKEN),
    );

    // ---- Page 1: from the start (after_idx unset), limit 3 → idx 0,1,2 ----
    let p1 = client
        .list_session_events(app::ListSessionEventsRequest {
            session_id: sid.to_string(),
            after_idx: None,
            limit: Some(3),
        })
        .await
        .expect("ListSessionEvents page 1")
        .into_inner();
    assert_eq!(
        p1.events.iter().map(|e| e.idx).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)],
        "page 1 idxs"
    );
    assert_eq!(p1.next_after_idx, 2, "page 1 cursor = last returned idx");

    // ---- Page 2: after_idx 2, limit 10 → the remaining idx 3,4 ----
    let p2 = client
        .list_session_events(app::ListSessionEventsRequest {
            session_id: sid.to_string(),
            after_idx: Some(p1.next_after_idx),
            limit: Some(10),
        })
        .await
        .expect("ListSessionEvents page 2")
        .into_inner();
    assert_eq!(
        p2.events.iter().map(|e| e.idx).collect::<Vec<_>>(),
        vec![Some(3), Some(4)],
        "page 2 idxs"
    );
    assert_eq!(p2.next_after_idx, 4, "page 2 cursor");
    assert_eq!(
        p2.events[1].kind, "run_completed",
        "kinds pass through verbatim (unfiltered)"
    );

    // ---- Page 3: at the tail → empty, cursor echoes (no rewind) ----
    let p3 = client
        .list_session_events(app::ListSessionEventsRequest {
            session_id: sid.to_string(),
            after_idx: Some(p2.next_after_idx),
            limit: Some(10),
        })
        .await
        .expect("ListSessionEvents page 3")
        .into_inner();
    assert!(p3.events.is_empty(), "tail read returns nothing");
    assert_eq!(p3.next_after_idx, 4, "tail cursor echoes, never rewinds");

    server.abort();
}
