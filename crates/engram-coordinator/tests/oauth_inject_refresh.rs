//! The egress-proxy refresh route over the connector-OAuth arm
//! (`POST /api/v1/hosts/:id/sessions/:sid/inject/refresh` with an
//! `oauth_connector` mint source): the coordinator resolves the sealed
//! bundle, refreshes inline when expired (against a fake provider), and
//! fails the route when the credential is absent so the proxy keeps its
//! stale secret.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use engram_coordinator::{api, AppState, CoordinatorConfig, Services};
use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::connector_oauth::{
    ConnectorOAuthBundle, ConnectorOAuthKind, ConnectorOAuthRefreshSpec,
};
use engram_core::types::oauth::{
    NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthSubjectKind,
};
use engram_crypto::{CredCipher, EnvVarKeyProvider};
use engram_host_agent::LocalHostClient;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    app: axum::Router,
    meta: Arc<SimMetadataStore>,
    clock: Arc<ManualClock>,
    kek: EnvVarKeyProvider,
}

fn fixture() -> Fixture {
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(24));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let sandbox_dir = tempfile::tempdir().expect("sandbox tempdir").keep();
    let services = Services {
        meta: meta.clone(),
        host: Arc::new(LocalHostClient::with_noop_hub(Arc::new(
            ProcessBackend::new(sandbox_dir),
        ))),
        secrets: Arc::new(InMemorySecretStore::with_secrets([
            ("linear.client_id", "client-123"),
            ("linear.client_secret", "secret-456"),
        ])),
        kek: Arc::new(EnvVarKeyProvider::from_bytes([0u8; 32], "test:v1")),
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
        clock: clock.clone(),
        entropy,
    };
    let state = Arc::new(AppState::new(CoordinatorConfig::default(), services));
    Fixture {
        app: api::router(state),
        meta,
        clock,
        kek: EnvVarKeyProvider::from_bytes([0u8; 32], "test:v1"),
    }
}

fn linear_key() -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-linear".into(),
        provider: "linear".into(),
    }
}

fn user_key() -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::UserConnector,
        subject_id: "user-7".into(),
        provider: "linear".into(),
    }
}

async fn seed_bundle(fx: &Fixture, bundle: &ConnectorOAuthBundle) {
    seed_bundle_for(fx, linear_key(), bundle).await;
}

async fn seed_bundle_for(fx: &Fixture, key: OAuthCredentialKey, bundle: &ConnectorOAuthBundle) {
    let sealed = CredCipher::new(&fx.kek)
        .seal(&bundle.to_json().expect("bundle json"))
        .await
        .expect("seal");
    fx.meta
        .put_oauth_credential(
            NewSealedOAuthCredential {
                key,
                wrapped_dek: sealed.wrapped_dek,
                nonce: sealed.nonce.to_vec(),
                ciphertext: sealed.ciphertext,
                key_id: sealed.key_id,
                metadata: OAuthAccountMetadata {
                    account_id: "app-user-1".into(),
                    display_name: None,
                    plan_type: None,
                    workspace_id: None,
                    workspace_name: None,
                },
                expires_at: bundle.expires_at,
            },
            None,
        )
        .await
        .expect("seed credential");
}

async fn refresh_call(app: axum::Router) -> (StatusCode, Value) {
    refresh_call_with(
        app,
        json!({
            "mint_source": {
                "oauth_connector": {
                    "connection_id": "conn-linear",
                    "provider": "linear",
                }
            }
        }),
    )
    .await
}

async fn refresh_call_with(app: axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/api/v1/hosts/{}/sessions/{}/inject/refresh",
            uuid::Uuid::from_u128(1),
            uuid::Uuid::from_u128(2),
        ))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn refresh_route_returns_the_sealed_raw_token() {
    let fx = fixture();
    seed_bundle(
        &fx,
        &ConnectorOAuthBundle {
            v: 1,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: "at-sealed".into(),
            token_type: "bearer".into(),
            refresh_token: None,
            scope: None,
            obtained_at: fx.clock.now_utc(),
            // Non-expiring bundle: resolution is read-only.
            expires_at: None,
            refresh: None,
        },
    )
    .await;
    let (status, body) = refresh_call(fx.app.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["secret"], "at-sealed");
}

#[tokio::test]
async fn refresh_route_refreshes_an_expired_bundle_against_the_provider() {
    // Fake token endpoint on loopback; the sealed refresh spec points at it.
    let provider_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = provider_hits.clone();
    let router = axum::Router::new().route(
        "/oauth/token",
        axum::routing::post(move || {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                axum::Json(json!({
                    "access_token": "at-rotated",
                    "token_type": "Bearer",
                    "expires_in": 86400,
                    "refresh_token": "rt-rotated",
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let fx = fixture();
    let now = fx.clock.now_utc();
    seed_bundle(
        &fx,
        &ConnectorOAuthBundle {
            v: 1,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: "at-stale".into(),
            token_type: "bearer".into(),
            refresh_token: Some("rt-stale".into()),
            scope: None,
            obtained_at: now - chrono::Duration::hours(24),
            expires_at: Some(now - chrono::Duration::hours(1)),
            refresh: Some(ConnectorOAuthRefreshSpec {
                token_url: format!("http://127.0.0.1:{}/oauth/token", addr.port()),
                client_id_ref: "linear.client_id".into(),
                client_secret_ref: "linear.client_secret".into(),
            }),
        },
    )
    .await;
    let (status, body) = refresh_call(fx.app.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["secret"], "at-rotated");
    assert_eq!(
        provider_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the expired seed triggered exactly one provider refresh"
    );
}

#[tokio::test]
async fn refresh_route_fails_when_no_credential_exists() {
    let fx = fixture();
    let (status, _body) = refresh_call(fx.app.clone()).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the proxy keeps its stale secret on a failed refresh"
    );
}

fn oauth_user_body() -> Value {
    json!({
        "mint_source": {
            "oauth_user": {
                "user_id": "user-7",
                "connection_id": "conn-linear",
                "provider": "linear",
            }
        }
    })
}

/// ADR 0115: an `oauth_user` mint source resolves the USER-subject sealed
/// credential. A static token (PAT) has no expiry, so the returned refresh
/// horizon clamps to 24 h — a disconnect or replacement reaches live
/// sessions within a day.
#[tokio::test]
async fn refresh_route_resolves_a_user_static_token_with_a_daily_horizon() {
    let fx = fixture();
    seed_bundle_for(
        &fx,
        user_key(),
        &ConnectorOAuthBundle {
            v: 1,
            kind: ConnectorOAuthKind::StaticToken,
            access_token: "personal-pat".into(),
            token_type: "bearer".into(),
            refresh_token: None,
            scope: None,
            obtained_at: fx.clock.now_utc(),
            expires_at: None,
            refresh: None,
        },
    )
    .await;
    let (status, body) = refresh_call_with(fx.app.clone(), oauth_user_body()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["secret"], "personal-pat");
    let expires: chrono::DateTime<chrono::Utc> = body["expires_at"]
        .as_str()
        .expect("expires_at")
        .parse()
        .expect("rfc3339");
    assert_eq!(
        expires,
        fx.clock.now_utc() + chrono::Duration::hours(24),
        "personal non-expiring tokens re-resolve daily"
    );
}

/// The user subject is keyed by user id, not connection id: a connector-
/// subject credential must not satisfy an `oauth_user` entry, and a missing
/// user credential fails the route so the proxy keeps its stale secret.
#[tokio::test]
async fn refresh_route_keeps_user_and_connector_subjects_apart() {
    let fx = fixture();
    seed_bundle(
        &fx,
        &ConnectorOAuthBundle {
            v: 1,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: "org-token".into(),
            token_type: "bearer".into(),
            refresh_token: None,
            scope: None,
            obtained_at: fx.clock.now_utc(),
            expires_at: None,
            refresh: None,
        },
    )
    .await;
    let (status, _body) = refresh_call_with(fx.app.clone(), oauth_user_body()).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an org connector credential must not satisfy a user-subject inject"
    );
}
