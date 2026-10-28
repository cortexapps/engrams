//! Service-account grants against a fake provider that verifies each JWT.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use engram_coordinator::oauth::{OAuthManager, OAuthServiceError};
use engram_coordinator::oauth_redirect::{RedirectMetadataProbe, RedirectMetadataSpec};
use engram_coordinator::oauth_service_account::ServiceAccountSpec;
use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::connector_oauth::{
    ConnectorOAuthBundle, ConnectorOAuthKind, TokenRequestBody,
};
use engram_core::types::oauth::{OAuthCredentialKey, OAuthSubjectKind};
use engram_crypto::{CredCipher, EnvVarKeyProvider, SealedCred};
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use serde_json::json;
use std::sync::OnceLock;

/// One recorded token request: its content type and its fields.
#[derive(Clone, Debug)]
struct TokenRequest {
    content_type: String,
    claims: serde_json::Value,
}

#[derive(Clone, Default)]
struct Provider {
    requests: Arc<Mutex<Vec<TokenRequest>>>,
    tokens_issued: Arc<Mutex<u32>>,
}

async fn serve(provider: Provider) -> String {
    let router = Router::new()
        .route(
            "/oauth/token",
            post(
                |State(p): State<Provider>, headers: HeaderMap, body: String| async move {
                    let content_type = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    assert!(content_type.starts_with("application/x-www-form-urlencoded"));
                    let fields: BTreeMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect();
                    assert_eq!(
                        fields["grant_type"],
                        "urn:ietf:params:oauth:grant-type:jwt-bearer"
                    );
                    let mut validation =
                        jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
                    // Time comes from ManualClock; check it below instead of the wall clock.
                    validation.validate_exp = false;
                    validation.set_audience(&[format!(
                        "http://{}/oauth/token",
                        headers["host"].to_str().unwrap()
                    )]);
                    let decoded = jsonwebtoken::decode::<serde_json::Value>(
                        &fields["assertion"],
                        &jsonwebtoken::DecodingKey::from_rsa_pem(test_keys().1.as_bytes()).unwrap(),
                        &validation,
                    )
                    .expect("valid signed assertion");
                    let claims = decoded.claims;
                    assert_eq!(claims["iss"], "service@example.com");
                    assert_eq!(claims["sub"], "principal@example.com");
                    assert_eq!(claims["scope"], "directory.read directory.list");
                    assert_eq!(
                        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
                        3600
                    );
                    p.requests.lock().push(TokenRequest {
                        content_type,
                        claims,
                    });
                    let mut issued = p.tokens_issued.lock();
                    *issued += 1;
                    Json(json!({
                        "access_token": format!("at-{}", *issued),
                        "token_type": "Bearer",
                        "expires_in": 3600,
                    }))
                },
            ),
        )
        .route(
            "/account",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer at-1");
                Json(json!({"name": "Directory account", "id": "account-1"}))
            }),
        )
        .with_state(provider);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake provider");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    format!("127.0.0.1:{}", addr.port())
}

struct Fixture {
    manager: Arc<OAuthManager>,
    meta: Arc<SimMetadataStore>,
    clock: Arc<ManualClock>,
    kek: EnvVarKeyProvider,
    host: String,
    provider: Provider,
}

async fn fixture() -> Fixture {
    let provider = Provider::default();
    let host = serve(provider.clone()).await;
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(1061));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let key_json = json!({
        "client_email": "service@example.com",
        "private_key": test_keys().0,
        "token_uri": format!("http://{host}/oauth/token"),
    })
    .to_string();
    let secrets = InMemorySecretStore::with_secrets([("directory.key", key_json)]);
    let manager = OAuthManager::new(
        meta.clone(),
        Arc::new(EnvVarKeyProvider::from_bytes([9u8; 32], "test:v1")),
        clock.clone(),
        entropy,
        Arc::new(secrets),
    );
    Fixture {
        manager,
        meta,
        clock,
        kek: EnvVarKeyProvider::from_bytes([9u8; 32], "test:v1"),
        host,
        provider,
    }
}

fn connector_key() -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-default".into(),
        provider: "directory".into(),
    }
}

fn test_keys() -> &'static (String, String) {
    static KEYS: OnceLock<(String, String)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let key = rsa::RsaPrivateKey::new(&mut rand_core06::OsRng, 2048).expect("RSA key");
        (
            key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
            key.to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap(),
        )
    })
}

fn spec(host: &str) -> ServiceAccountSpec {
    ServiceAccountSpec {
        token_url: format!("http://{host}/oauth/token"),
        scopes: vec!["directory.read".into(), "directory.list".into()],
        scope_delimiter: String::new(),
        key_ref: "directory.key".into(),
        subject: Some("principal@example.com".into()),
        metadata: RedirectMetadataSpec::default(),
    }
}

async fn open_bundle(fx: &Fixture, key: &OAuthCredentialKey) -> ConnectorOAuthBundle {
    let row = fx
        .meta
        .get_oauth_credential(key)
        .await
        .expect("get")
        .expect("credential row");
    let nonce: [u8; 12] = row.nonce.as_slice().try_into().expect("nonce");
    let payload = CredCipher::new(&fx.kek)
        .open(&SealedCred {
            wrapped_dek: row.wrapped_dek.clone(),
            nonce,
            ciphertext: row.ciphertext.clone(),
            key_id: row.key_id.clone(),
        })
        .await
        .expect("open bundle");
    ConnectorOAuthBundle::from_json(&payload).expect("parse bundle")
}

#[tokio::test]
async fn grant_seals_a_re_grantable_bundle_with_the_principal() {
    let fx = fixture().await;
    let row = fx
        .manager
        .grant_service_account(&connector_key(), &spec(&fx.host))
        .await
        .unwrap();
    assert_eq!(
        row.metadata.display_name.as_deref(),
        Some("principal@example.com")
    );
    assert_eq!(
        row.expires_at,
        Some(fx.clock.now_utc() + chrono::Duration::seconds(3600))
    );
    let requests = fx.provider.requests.lock().clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .content_type
        .starts_with("application/x-www-form-urlencoded"));
    assert_eq!(requests[0].claims["iat"], fx.clock.now_utc().timestamp());
    let bundle = open_bundle(&fx, &connector_key()).await;
    assert_eq!(bundle.kind, ConnectorOAuthKind::Oauth2JwtBearer);
    assert_eq!(bundle.refresh_token, None);
    assert!(bundle.refreshable());
    let refresh = bundle.refresh.unwrap();
    assert_eq!(refresh.assertion_key_ref.as_deref(), Some("directory.key"));
    assert_eq!(refresh.subject.as_deref(), Some("principal@example.com"));
    assert_eq!(
        refresh.scope.as_deref(),
        Some("directory.read directory.list")
    );
    assert_eq!(refresh.token_url, spec(&fx.host).token_url);
    assert_eq!(refresh.body, TokenRequestBody::Form);
    assert!(refresh.client_id_ref.is_empty());
    assert!(refresh.client_secret_ref.is_empty());
}

#[tokio::test]
async fn an_expiring_token_is_granted_again_on_resolution() {
    let fx = fixture().await;
    fx.manager
        .grant_service_account(&connector_key(), &spec(&fx.host))
        .await
        .unwrap();
    fx.clock.advance(std::time::Duration::from_secs(600));
    assert_eq!(
        fx.manager
            .resolve_connector_token(&connector_key())
            .await
            .unwrap()
            .secret,
        "at-1"
    );
    assert_eq!(fx.provider.requests.lock().len(), 1);
    fx.clock.advance(std::time::Duration::from_secs(1800));
    assert_eq!(
        fx.manager
            .resolve_connector_token(&connector_key())
            .await
            .unwrap()
            .secret,
        "at-2"
    );
    let requests = fx.provider.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].claims["iat"], fx.clock.now_utc().timestamp());
    assert_eq!(
        open_bundle(&fx, &connector_key()).await.access_token,
        "at-2"
    );
}

#[tokio::test]
async fn sweep_signs_a_new_assertion() {
    let fx = fixture().await;
    fx.manager
        .grant_service_account(&connector_key(), &spec(&fx.host))
        .await
        .unwrap();
    fx.clock.advance(std::time::Duration::from_secs(2400));
    assert_eq!(
        fx.manager
            .run_connector_refresh_once()
            .await
            .unwrap()
            .refreshed,
        1
    );
    assert_eq!(
        open_bundle(&fx, &connector_key()).await.access_token,
        "at-2"
    );
    assert_eq!(fx.provider.requests.lock().len(), 2);
}

#[tokio::test]
async fn re_grant_without_a_principal_reuses_the_sealed_one() {
    let fx = fixture().await;
    let first = fx
        .manager
        .grant_service_account(&connector_key(), &spec(&fx.host))
        .await
        .unwrap();
    let mut reconnect = spec(&fx.host);
    for subject in [None, Some(String::new())] {
        reconnect.subject = subject;
        let row = fx
            .manager
            .grant_service_account(&connector_key(), &reconnect)
            .await
            .unwrap();
        assert!(row.version > first.version);
        assert_eq!(row.metadata.display_name, first.metadata.display_name);
        assert_eq!(
            open_bundle(&fx, &connector_key())
                .await
                .refresh
                .unwrap()
                .subject
                .as_deref(),
            Some("principal@example.com")
        );
    }
}

#[tokio::test]
async fn a_key_for_a_different_token_uri_is_rejected() {
    let fx = fixture().await;
    let mut wrong = spec(&fx.host);
    wrong.token_url = "https://other.example.com/token".into();
    let err = fx
        .manager
        .grant_service_account(&connector_key(), &wrong)
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthServiceError::BadRequest(message) if message.contains("token_uri")));
    assert!(fx.provider.requests.lock().is_empty());
}

#[tokio::test]
async fn a_non_connector_subject_is_rejected() {
    let fx = fixture().await;
    for subject_kind in [
        OAuthSubjectKind::User,
        OAuthSubjectKind::UserConnector,
        OAuthSubjectKind::Mcp,
    ] {
        let key = OAuthCredentialKey {
            subject_kind,
            ..connector_key()
        };
        assert!(matches!(
            fx.manager
                .grant_service_account(&key, &spec(&fx.host))
                .await,
            Err(OAuthServiceError::BadRequest(_))
        ));
    }
    assert!(fx.provider.requests.lock().is_empty());
}

#[tokio::test]
async fn the_first_grant_requires_a_principal() {
    let fx = fixture().await;
    let mut missing = spec(&fx.host);
    for subject in [None, Some(String::new())] {
        missing.subject = subject;
        let err = fx
            .manager
            .grant_service_account(&connector_key(), &missing)
            .await
            .unwrap_err();
        assert!(
            matches!(err, OAuthServiceError::BadRequest(message) if message == "a principal is required")
        );
    }
    assert!(fx.provider.requests.lock().is_empty());
}

#[tokio::test]
async fn metadata_probe_can_override_the_display_name() {
    let fx = fixture().await;
    let mut settings = spec(&fx.host);
    settings.metadata.probe = Some(RedirectMetadataProbe {
        method: "GET".into(),
        host: fx.host.clone(),
        path: "/account".into(),
        body: String::new(),
        map: BTreeMap::from([
            ("display_name".into(), "name".into()),
            ("account_id".into(), "id".into()),
        ]),
    });
    let row = fx
        .manager
        .grant_service_account(&connector_key(), &settings)
        .await
        .unwrap();
    assert_eq!(
        row.metadata.display_name.as_deref(),
        Some("Directory account")
    );
    assert_eq!(row.metadata.account_id, "account-1");
}
