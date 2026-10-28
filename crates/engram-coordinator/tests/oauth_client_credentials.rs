//! The client-credentials grant family against a fake provider: the JSON
//! grant body Vanta takes, the form body the RFC describes, the sealed
//! bundle shape, and the re-grant that serves an expired token through the
//! resolution seam. No real provider is contacted.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use engram_coordinator::oauth::{OAuthManager, OAuthServiceError};
use engram_coordinator::oauth_client_credentials::ClientCredentialsSpec;
use engram_coordinator::oauth_redirect::RedirectMetadataSpec;
use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::connector_oauth::{
    ConnectorOAuthBundle, ConnectorOAuthKind, TokenRequestBody,
};
use engram_core::types::oauth::{OAuthCredentialKey, OAuthSubjectKind};
use engram_crypto::{CredCipher, EnvVarKeyProvider, SealedCred};
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;
use serde_json::{json, Value};

/// One recorded token request: its content type and its fields.
#[derive(Clone, Debug)]
struct TokenRequest {
    content_type: String,
    fields: BTreeMap<String, String>,
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
                    let fields: BTreeMap<String, String> =
                        if content_type.starts_with("application/json") {
                            serde_json::from_str::<BTreeMap<String, Value>>(&body)
                                .expect("json body")
                                .into_iter()
                                .map(|(k, v)| (k, v.as_str().unwrap_or("").to_owned()))
                                .collect()
                        } else {
                            url::form_urlencoded::parse(body.as_bytes())
                                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                .collect()
                        };
                    p.requests.lock().push(TokenRequest {
                        content_type,
                        fields,
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
    let secrets = InMemorySecretStore::with_secrets([
        ("vanta.client_id", "vci_123"),
        ("vanta.client_secret", "vcs_456"),
    ]);
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

fn vanta_key() -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-default".into(),
        provider: "vanta".into(),
    }
}

fn spec(host: &str, body: TokenRequestBody) -> ClientCredentialsSpec {
    ClientCredentialsSpec {
        token_url: format!("http://{host}/oauth/token"),
        scopes: vec!["vanta-api.all:read".into(), "vanta-api.all:write".into()],
        scope_delimiter: String::new(),
        client_id_ref: "vanta.client_id".into(),
        client_secret_ref: "vanta.client_secret".into(),
        body,
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
async fn grant_posts_a_json_body_and_seals_a_re_grantable_bundle() {
    let fx = fixture().await;
    let row = fx
        .manager
        .grant_client_credentials(&vanta_key(), &spec(&fx.host, TokenRequestBody::Json))
        .await
        .expect("grant");
    assert_eq!(row.key, vanta_key());
    assert_eq!(
        row.expires_at,
        Some(fx.clock.now_utc() + chrono::Duration::seconds(3600))
    );

    let requests = fx.provider.requests.lock().clone();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.content_type.starts_with("application/json"),
        "{}",
        request.content_type
    );
    assert_eq!(request.fields["grant_type"], "client_credentials");
    assert_eq!(request.fields["client_id"], "vci_123");
    assert_eq!(request.fields["client_secret"], "vcs_456");
    assert_eq!(
        request.fields["scope"],
        "vanta-api.all:read vanta-api.all:write"
    );

    let bundle = open_bundle(&fx, &vanta_key()).await;
    assert_eq!(bundle.kind, ConnectorOAuthKind::Oauth2ClientCredentials);
    assert_eq!(bundle.access_token, "at-1");
    assert_eq!(bundle.refresh_token, None);
    assert!(
        bundle.refreshable(),
        "the sealed spec lets the grant run again"
    );
    let refresh = bundle.refresh.expect("refresh spec");
    assert_eq!(refresh.body, TokenRequestBody::Json);
    assert_eq!(
        refresh.scope.as_deref(),
        Some("vanta-api.all:read vanta-api.all:write")
    );
}

#[tokio::test]
async fn grant_posts_a_form_when_the_spec_says_so() {
    let fx = fixture().await;
    fx.manager
        .grant_client_credentials(&vanta_key(), &spec(&fx.host, TokenRequestBody::Form))
        .await
        .expect("grant");
    let requests = fx.provider.requests.lock().clone();
    assert!(
        requests[0]
            .content_type
            .starts_with("application/x-www-form-urlencoded"),
        "{}",
        requests[0].content_type
    );
    assert_eq!(requests[0].fields["grant_type"], "client_credentials");
}

#[tokio::test]
async fn an_expiring_token_is_granted_again_on_resolution() {
    let fx = fixture().await;
    fx.manager
        .grant_client_credentials(&vanta_key(), &spec(&fx.host, TokenRequestBody::Json))
        .await
        .expect("grant");

    // Well inside the hour: the sealed token serves as is.
    fx.clock.advance(std::time::Duration::from_secs(10 * 60));
    let token = fx
        .manager
        .resolve_connector_token(&vanta_key())
        .await
        .expect("resolve");
    assert_eq!(token.secret, "at-1");
    assert_eq!(fx.provider.requests.lock().len(), 1);

    // Past expiry: the grant runs again from the sealed spec, with the
    // same JSON body and scope, and the new token is served.
    fx.clock.advance(std::time::Duration::from_secs(60 * 60));
    let token = fx
        .manager
        .resolve_connector_token(&vanta_key())
        .await
        .expect("resolve after expiry");
    assert_eq!(token.secret, "at-2");
    let requests = fx.provider.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].content_type.starts_with("application/json"));
    assert_eq!(requests[1].fields["grant_type"], "client_credentials");
    assert_eq!(
        requests[1].fields["scope"],
        "vanta-api.all:read vanta-api.all:write"
    );
    assert!(!requests[1].fields.contains_key("refresh_token"));

    let bundle = open_bundle(&fx, &vanta_key()).await;
    assert_eq!(bundle.access_token, "at-2");
    assert_eq!(bundle.kind, ConnectorOAuthKind::Oauth2ClientCredentials);
    assert!(bundle.refreshable());
}

#[tokio::test]
async fn a_second_grant_replaces_the_credential() {
    let fx = fixture().await;
    let first = fx
        .manager
        .grant_client_credentials(&vanta_key(), &spec(&fx.host, TokenRequestBody::Json))
        .await
        .expect("first grant");
    let second = fx
        .manager
        .grant_client_credentials(&vanta_key(), &spec(&fx.host, TokenRequestBody::Json))
        .await
        .expect("second grant");
    assert!(second.version > first.version);
    assert_eq!(open_bundle(&fx, &vanta_key()).await.access_token, "at-2");
}

#[tokio::test]
async fn grants_are_restricted_to_connector_subjects_and_sound_specs() {
    let fx = fixture().await;
    let user_key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::UserConnector,
        subject_id: "user-1".into(),
        provider: "vanta".into(),
    };
    let err = fx
        .manager
        .grant_client_credentials(&user_key, &spec(&fx.host, TokenRequestBody::Json))
        .await
        .expect_err("user subject");
    assert!(matches!(err, OAuthServiceError::BadRequest(_)), "{err:?}");

    let mut bad = spec(&fx.host, TokenRequestBody::Json);
    bad.client_id_ref = String::new();
    let err = fx
        .manager
        .grant_client_credentials(&vanta_key(), &bad)
        .await
        .expect_err("missing ref");
    assert!(matches!(err, OAuthServiceError::BadRequest(_)), "{err:?}");
    assert!(
        fx.provider.requests.lock().is_empty(),
        "nothing reached the provider"
    );
}
