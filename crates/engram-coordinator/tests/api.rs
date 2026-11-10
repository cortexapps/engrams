//! Integration test for the coordinator HTTP API.
//!
//! Wires the real `axum` router against an in-memory `MetadataStore`,
//! a `MockCloud`, a `LocalStorage` blob backend, and `ProcessBackend`
//! (the dev-loop SandboxBackend that runs commands as host
//! subprocesses), then drives the surface via
//! `tower::ServiceExt::oneshot`. The goal is to lock down the public
//! contract — status codes, JSON error envelope, the routing table —
//! while exercising real exec round-trips end-to-end.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use engram_coordinator::{api, AppState, CoordinatorConfig, Services};
use engram_core::traits::MetadataStore;
use engram_core::types::{HostRecord, HostStatus};
use engram_core::{HostId, SessionId};
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{SimEntropy, SimMetadataStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

// Sessions are staged through real `SimMetadataStore` calls, never a
// hand-rolled mock.

fn sim_meta() -> Arc<SimMetadataStore> {
    SimMetadataStore::new(
        Arc::new(engram_core::traits::SystemClock::new()),
        Arc::new(SimEntropy::seeded(0xA912)),
    )
}

// ---------------------------------------------------------------------
// Fixture: build a fully-wired axum router against in-memory components.
// ---------------------------------------------------------------------

async fn build_app(meta: Arc<SimMetadataStore>) -> axum::Router {
    TestFixture::new(meta, InMemorySecretStore::new()).await.app
}

/// Like `build_app` but seeds the bearer-token allow-list. Used by the
/// auth middleware tests; everything else relies on the default empty
/// list (auth-disabled).
fn build_app_with_tokens(meta: Arc<SimMetadataStore>, tokens: Vec<String>) -> axum::Router {
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta,
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
        secrets: Arc::new(InMemorySecretStore::new()),
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
        clock: Arc::new(engram_core::traits::SystemClock::new()),
        entropy: Arc::new(engram_core::traits::OsEntropy),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        auth_tokens: tokens,
        ..CoordinatorConfig::default()
    };
    let state = Arc::new(AppState::new(cfg, services));
    api::router(state)
}

/// ADR 0023: build a router with a configured (mock) git forge + a
/// seeded session whose credential-broker token is `broker-tok-123`.
/// Returns the router, the session id, and the forge handle (so tests
/// can assert recorded change requests).
async fn build_forge_app() -> (
    axum::Router,
    SessionId,
    Arc<engram_git_dev::StaticGitHubIntegration>,
) {
    let meta = sim_meta();
    let session_id = meta
        .create_session(engram_core::types::session::SessionSpec {
            image: "cortexapps/engrams:warm-bootstrap".to_string(),
            mode: Default::default(),
        })
        .await
        .expect("seed session");
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let forge = Arc::new(engram_git_dev::StaticGitHubIntegration::github(
        "ghs_test_xyz",
    ));
    let services = Services {
        meta,
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
        blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test"),
        )),
        chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
            engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            ),
        )),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
        clock: Arc::new(engram_core::traits::SystemClock::new()),
        entropy: Arc::new(engram_core::traits::OsEntropy),
    };
    let cfg = CoordinatorConfig {
        default_image_version: "warm-bootstrap".into(),
        ..CoordinatorConfig::default()
    };
    let mut app = AppState::new(cfg, services);
    app.integrations = engram_coordinator::integrations::IntegrationBroker::with(forge.clone());
    let state = Arc::new(app);
    state
        .git_broker_tokens
        .insert(session_id, "broker-tok-123".to_string());
    (api::router(state), session_id, forge)
}

#[tokio::test]
async fn forge_git_credential_gated_by_broker_token() {
    let (app, sid, _forge) = build_forge_app().await;
    let uri = format!("/api/v1/sessions/{sid}/git-credential");

    // No token → 401.
    let resp = app
        .clone()
        .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let resp = app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header("authorization", "Bearer wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Valid broker token → 200 + the minted credential.
    let resp = app
        .oneshot(
            Request::get(&uri)
                .header("authorization", "Bearer broker-tok-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["username"], "x-access-token");
    assert_eq!(v["password"], "ghs_test_xyz");
}

#[tokio::test]
async fn forge_forward_runs_the_core_for_split_hosts() {
    // ADR 0023 split-mode forwarding: an FC host can't run the forge sink
    // locally, so it POSTs the in-guest ForgeRequest (with its broker
    // token) to /api/hosts/forge and gets back the same ForgeResponse the
    // vsock path would produce.
    let (app, sid, _forge) = build_forge_app().await;

    // Valid broker token in the body → a minted credential.
    let req = engram_harness_proto::ForgeRequest {
        session_id: sid,
        broker_token: "broker-tok-123".to_string(),
        op: engram_harness_proto::ForgeOp::FetchCredential {
            host: "github.com".to_string(),
            owner: None,
        },
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/hosts/forge")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["Credential"]["password"]
            .as_str()
            .is_some_and(|p| !p.is_empty()),
        "expected a minted credential, got {v:?}",
    );

    // Wrong broker token → ForgeResponse::Error (200, error in the body),
    // never a minted credential — the body token is still validated.
    let bad = engram_harness_proto::ForgeRequest {
        session_id: sid,
        broker_token: "wrong".to_string(),
        op: engram_harness_proto::ForgeOp::FetchCredential {
            host: "github.com".to_string(),
            owner: None,
        },
    };
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/hosts/forge")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&bad).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v.get("Error").is_some(),
        "bad token must yield Error, got {v:?}"
    );
}

/// Fully-wired axum router over in-memory components, exposing the
/// pinned in-process host id (the heartbeat test targets it).
struct TestFixture {
    app: axum::Router,
    /// ADR 0015 M5: the single in-process host the fixture registers +
    /// seeds a fresh Ready row for.
    test_host_id: HostId,
}

impl TestFixture {
    async fn new(meta: Arc<SimMetadataStore>, secrets: InMemorySecretStore) -> Self {
        // Separate tempdirs for each on-disk component so they can't
        // accidentally collide. All leak (`keep()`) because the axum
        // router needs to outlive this function — the OS cleans up
        // `/tmp` later.
        let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
        // Match production wiring: the host-side PooledBackend wraps
        // the real backend so the chunked-OCI / image-cache / egress
        // helpers fire the same way the multi-host setup runs them.
        let raw: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_dir));
        let backend: Arc<dyn engram_core::traits::SandboxBackend> =
            Arc::new(engram_host_agent::pooled_backend::PooledBackend::new(raw));
        let services = Services {
            meta: meta.clone(),
            host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(backend)),
            secrets: Arc::new(secrets),
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
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = CoordinatorConfig {
            default_image_version: "warm-bootstrap".into(),
            ..CoordinatorConfig::default()
        };
        let host_registry = Arc::new(engram_coordinator::HostRegistry::new(meta.clone()));
        let test_host_id = HostId::new();
        host_registry.register(test_host_id, services.host.clone());
        // ADR 0047: placement + the heartbeat handler read host ROWS —
        // seed a fresh, Ready row for the test host through the real
        // upsert path.
        meta.upsert_host(HostRecord {
            id: test_host_id,
            hostname: "api-test-host".into(),
            cloud_metadata: Default::default(),
            capacity: engram_core::types::HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 16_384,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: Default::default(),
            status: HostStatus::Ready,
            last_heartbeat_at: Utc::now(),
            host_addr: None,
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            sandbox_bundles: Vec::new(),
            cordoned: false,
            cordon_owner: None,
            cordon_reason: None,
            retire_requested_at: None,
            retired_at: None,
            total_vcpus: 0,
            wire_version: 0,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
            lease_expires_at: None,
            lease_state: Default::default(),
            lease_epoch: 0,
        })
        .await
        .expect("seed test host row");
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        Self {
            app: api::router(state),
            test_host_id,
        }
    }
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "response body was not valid JSON: {e}; body = {:?}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Bearer-token middleware
// ---------------------------------------------------------------------

// ADR 0031: the deployment bearer no longer gates *human* routes — those are
// authenticated by the principal layer (cookie / service-bearer / synthetic).
// The bearer now guards only the `internal` control-plane router (host →
// coord ingestion). These tests target an internal route (`/hosts/register`)
// to exercise that gate. A separate test below confirms human routes are
// principal-authed (synthetic admin → 200) regardless of the bearer.

const INTERNAL_ROUTE: &str = "/api/v1/hosts/register";

#[tokio::test]
async fn auth_rejects_internal_request_without_authorization_header() {
    let app = build_app_with_tokens(sim_meta(), vec!["alpha".into()]);
    let resp = app
        .oneshot(json_request(Method::POST, INTERNAL_ROUTE, json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn auth_rejects_wrong_token_on_internal_route() {
    let app = build_app_with_tokens(sim_meta(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Bearer beta")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp.into_body()).await;
    assert!(v["message"].as_str().unwrap().contains("invalid"));
}

#[tokio::test]
async fn auth_rejects_non_bearer_scheme_on_internal_route() {
    let app = build_app_with_tokens(sim_meta(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Basic YWxwaGE=")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_valid_bearer_passes_internal_gate() {
    // A valid token clears the bearer layer; the handler then runs (and may
    // reject the empty body) — the point is it is NOT 401.
    let app = build_app_with_tokens(sim_meta(), vec!["alpha".into()]);
    let resp = app
        .oneshot(
            Request::post(INTERNAL_ROUTE)
                .header("authorization", "Bearer alpha")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "valid bearer must clear the internal gate"
    );
}

#[tokio::test]
async fn auth_lets_healthz_through_without_token() {
    // Liveness probes don't carry secrets — `/healthz` must stay
    // outside the auth layer even when auth is enabled.
    let app = build_app_with_tokens(sim_meta(), vec!["alpha".into()]);
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn healthz_returns_ok_status_and_version() {
    let app = build_app(sim_meta()).await;
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["status"], "ok");
    assert!(v["version"].is_string());
}

// ---------------------------------------------------------------------
// End-to-end exec wiring (POST /sessions → registry → sandbox.exec)
//
// The registry is in-process state owned by AppState. Tests that need
// create→exec to share state must reuse the *same* `axum::Router`
// across requests (cloning the Router is fine — it shares the
// Arc<AppState> internally).
// ---------------------------------------------------------------------

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = build_app(sim_meta()).await;
    let resp = app
        .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Issue #231 regression: the heartbeat handler must NOT swallow a
/// `touch_host_heartbeat` persist failure into a 200. If it does, the
/// host's `last_heartbeat_at` row goes stale while the host keeps
/// getting 200-acks (so it never retries/backs off), and a sibling
/// coord pod's dead-host detector then orphans every session on a host
/// that is alive and loaded. The contract: persist failure ⇒ 5xx so
/// the host's loop backs off and the failure is visible.
#[tokio::test]
async fn heartbeat_persist_failure_returns_5xx_not_swallowed_200() {
    let store = sim_meta();
    let f = TestFixture::new(store.clone(), InMemorySecretStore::new()).await;
    let host_id = f.test_host_id;
    let app = f.app;

    let hb_body = json!({
        "capacity": { "total_mib": 16384u64, "used_mib": 0u64, "running_sandboxes": 0u32 },
    });

    // Persist fails (a saturated pool / PG outage) → the handler must
    // return a 5xx, NOT a 200. This is the regression: pre-fix the error
    // was only logged and the handler fell through to a 200 ack. The
    // faithful store models this as a whole-store outage window (the
    // retired mock had a per-method `touch_host_heartbeat` fail flag);
    // either way the heartbeat handler's single persist errors.
    store.set_outage(true);
    let resp = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/hosts/{host_id}/heartbeat"),
            hb_body.clone(),
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_server_error(),
        "persist failure must surface as 5xx so the host backs off; got {}",
        resp.status()
    );

    // Control: with the persist healthy, the *same* request acks 200 —
    // proving the 5xx above is caused specifically by the persist
    // failure, not by an unrelated handler error.
    store.set_outage(false);
    let resp = app
        .oneshot(json_request(
            Method::POST,
            &format!("/api/v1/hosts/{host_id}/heartbeat"),
            hb_body,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "healthy persist should ack 200",
    );
}
