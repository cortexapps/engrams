//! ADR 0115: user-scoped connector credentials in the sealed store —
//! static tokens (PATs) under `user_connector` subjects, the deliberate
//! PAT↔OAuth replacement semantics, sweep coverage for user-subject OAuth,
//! and the structural invisibility of static tokens to the refresh rail.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use engram_coordinator::oauth::{OAuthManager, OAuthServiceError};
use engram_coordinator::oauth_redirect::{
    RedirectMetadataProbe, RedirectMetadataSpec, RedirectOauthSpec, META_ACCOUNT_ID,
};
use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::connector_oauth::{ConnectorOAuthBundle, ConnectorOAuthKind};
use engram_core::types::oauth::{OAuthCredentialKey, OAuthCredentialStatus, OAuthSubjectKind};
use engram_crypto::{CredCipher, EnvVarKeyProvider, SealedCred};
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Provider {
    token_response: Arc<Mutex<Value>>,
    viewer_response: Arc<Mutex<Value>>,
}

async fn serve(provider: Provider) -> String {
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|State(p): State<Provider>, _body: String| async move {
                Json(p.token_response.lock().clone())
            }),
        )
        .route(
            "/graphql",
            post(|State(p): State<Provider>| async move { Json(p.viewer_response.lock().clone()) }),
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
    *provider.token_response.lock() = json!({
        "access_token": "at-1",
        "token_type": "Bearer",
        "expires_in": 86400,
        "refresh_token": "rt-1",
        "scope": "read,write",
    });
    *provider.viewer_response.lock() = json!({
        "data": {"viewer": {"id": "acct-person-1"}}
    });
    let host = serve(provider.clone()).await;

    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(1150));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let secrets = InMemorySecretStore::with_secrets([
        ("linear.client_id", "client-123"),
        ("linear.client_secret", "secret-456"),
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

fn user_key(provider: &str) -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::UserConnector,
        subject_id: "user-1".into(),
        provider: provider.into(),
    }
}

fn spec(host: &str) -> RedirectOauthSpec {
    RedirectOauthSpec {
        authorize_url: "https://linear.app/oauth/authorize".into(),
        token_url: format!("http://{host}/oauth/token"),
        scopes: vec!["read".into(), "write".into()],
        scope_delimiter: String::new(),
        extra_authorize_params: BTreeMap::new(),
        client_id_ref: "linear.client_id".into(),
        client_secret_ref: "linear.client_secret".into(),
        pkce: false,
        metadata: RedirectMetadataSpec {
            from_token_response: BTreeMap::new(),
            probe: Some(RedirectMetadataProbe {
                method: "POST".into(),
                host: host.to_string(),
                path: "/graphql".into(),
                body: r#"{"query":"{ viewer { id } }"}"#.into(),
                map: BTreeMap::from([(META_ACCOUNT_ID.to_string(), "data.viewer.id".to_string())]),
            }),
        },
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

async fn connect_user_oauth(fx: &Fixture, sp: &RedirectOauthSpec) {
    let begun = fx
        .manager
        .begin_redirect(user_key("linear"), sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    fx.manager
        .complete_redirect(
            user_key("linear"),
            begun.flow.id,
            "code-abc",
            "https://cb.example.com/x",
            sp,
        )
        .await
        .expect("complete");
}

#[tokio::test]
async fn put_static_credential_seals_resolves_and_replaces() {
    let fx = fixture().await;
    let key = user_key("sentry");

    let row = fx
        .manager
        .put_static_credential(&key, "  pat-secret-1  ")
        .await
        .expect("put");
    assert_eq!(
        row.status(fx.clock.now_utc()),
        OAuthCredentialStatus::Connected
    );
    assert_eq!(row.expires_at, None);
    assert_eq!(row.metadata.account_id, "", "no provider-verified identity");

    let resolved = fx
        .manager
        .resolve_connector_token(&key)
        .await
        .expect("resolve");
    assert_eq!(resolved.secret, "pat-secret-1", "value is trimmed");
    assert_eq!(resolved.expires_at, None);

    let bundle = open_bundle(&fx, &key).await;
    assert_eq!(bundle.kind, ConnectorOAuthKind::StaticToken);
    assert!(!bundle.refreshable());

    // Replacement is a CAS write over the current version.
    let replaced = fx
        .manager
        .put_static_credential(&key, "pat-secret-2")
        .await
        .expect("replace");
    assert!(replaced.version > row.version);
    let resolved = fx
        .manager
        .resolve_connector_token(&key)
        .await
        .expect("resolve replacement");
    assert_eq!(resolved.secret, "pat-secret-2");
}

#[tokio::test]
async fn put_static_credential_rejects_bad_subjects_and_empty_values() {
    let fx = fixture().await;
    let connector_key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-1".into(),
        provider: "sentry".into(),
    };
    let err = fx
        .manager
        .put_static_credential(&connector_key, "pat")
        .await
        .expect_err("connector subjects must be rejected");
    assert!(matches!(err, OAuthServiceError::BadRequest(_)));

    let err = fx
        .manager
        .put_static_credential(&user_key("sentry"), "   ")
        .await
        .expect_err("blank values must be rejected");
    assert!(matches!(err, OAuthServiceError::BadRequest(_)));
}

#[tokio::test]
async fn static_tokens_never_enter_the_refresh_sweep() {
    let fx = fixture().await;
    let key = user_key("sentry");
    fx.manager
        .put_static_credential(&key, "pat-secret-1")
        .await
        .expect("put");

    // Even far in the future the PAT has no expiry: never due, never broken.
    fx.clock
        .advance(std::time::Duration::from_secs(30 * 24 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(sweep.examined, 0, "{sweep:?}");

    let row = fx
        .meta
        .get_oauth_credential(&key)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.broken_at, None);
    assert_eq!(
        fx.manager
            .resolve_connector_token(&key)
            .await
            .expect("resolve")
            .secret,
        "pat-secret-1"
    );
}

#[tokio::test]
async fn user_subject_oauth_lands_under_user_connector_and_sweep_refreshes_it() {
    let fx = fixture().await;
    let sp = spec(&fx.host);
    connect_user_oauth(&fx, &sp).await;

    let listed = fx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::UserConnector, Some("user-1"))
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, user_key("linear"));
    assert_eq!(listed[0].metadata.account_id, "acct-person-1");

    // Inside the 6h margin of the 24h token, the sweep refreshes the
    // user-subject row — proof the rail covers UserConnector.
    *fx.provider.token_response.lock() = json!({
        "access_token": "at-2",
        "token_type": "Bearer",
        "expires_in": 86400,
        "refresh_token": "rt-2",
    });
    fx.clock.advance(std::time::Duration::from_secs(19 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(sweep.refreshed, 1, "{sweep:?}");
    let bundle = open_bundle(&fx, &user_key("linear")).await;
    assert_eq!(bundle.access_token, "at-2");
    assert_eq!(bundle.refresh_token.as_deref(), Some("rt-2"));
}

#[tokio::test]
async fn pat_and_oauth_deliberately_replace_each_other() {
    let fx = fixture().await;
    let key = user_key("linear");
    let sp = spec(&fx.host);

    // PAT first; an OAuth connect replaces it (empty stored account id
    // records no identity to protect).
    fx.manager
        .put_static_credential(&key, "pat-secret-1")
        .await
        .expect("put PAT");
    connect_user_oauth(&fx, &sp).await;
    let bundle = open_bundle(&fx, &key).await;
    assert_eq!(bundle.kind, ConnectorOAuthKind::Oauth2AuthorizationCode);
    assert_eq!(bundle.access_token, "at-1");

    // A PAT write replaces the OAuth credential without an account check:
    // the user pasted it on purpose.
    fx.manager
        .put_static_credential(&key, "pat-secret-2")
        .await
        .expect("PAT over OAuth");
    let bundle = open_bundle(&fx, &key).await;
    assert_eq!(bundle.kind, ConnectorOAuthKind::StaticToken);

    // OAuth-over-OAuth account-switch protection is still intact.
    connect_user_oauth(&fx, &sp).await;
    *fx.provider.viewer_response.lock() = json!({
        "data": {"viewer": {"id": "acct-other-person"}}
    });
    let begun = fx
        .manager
        .begin_redirect(key.clone(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let err = fx
        .manager
        .complete_redirect(
            key.clone(),
            begun.flow.id,
            "code-abc",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect_err("account switch must be rejected");
    assert_eq!(err.code(), "account_changed");
}

#[tokio::test]
async fn lookup_flow_returns_the_subject_without_a_fence() {
    let fx = fixture().await;
    let sp = spec(&fx.host);
    let begun = fx
        .manager
        .begin_redirect(user_key("linear"), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");

    let flow = fx.manager.lookup_flow(begun.flow.id).await.expect("lookup");
    assert_eq!(flow.key, user_key("linear"));

    let missing = fx
        .manager
        .lookup_flow(uuid::Uuid::from_u128(0xdead_beef))
        .await
        .expect_err("unknown flow id");
    assert!(matches!(missing, OAuthServiceError::NotFound));
}
