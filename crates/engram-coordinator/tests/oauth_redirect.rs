//! The generic authorization-code flow family against a fake provider:
//! RFC-shaped grants, Slack's HTTP-200 `{"ok":false}` failure body, Linear's
//! array `scope`, PKCE derivation, the durable-flow CSRF fence, and stale-
//! attempt supersession. No real provider is contacted.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use engram_coordinator::oauth::OAuthManager;
use engram_coordinator::oauth_redirect::{
    RedirectMetadataProbe, RedirectMetadataSpec, RedirectOauthSpec, META_ACCOUNT_ID,
    META_DISPLAY_NAME, META_WORKSPACE_ID, META_WORKSPACE_NAME,
};
use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::connector_oauth::ConnectorOAuthBundle;
use engram_core::types::oauth::{OAuthCredentialKey, OAuthFlowStatus, OAuthSubjectKind};
use engram_crypto::{CredCipher, EnvVarKeyProvider, SealedCred};
use engram_secrets_dev::InMemorySecretStore;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;
use serde_json::{json, Value};

/// What the fake provider should answer, plus a capture of what it saw.
#[derive(Clone, Default)]
struct Provider {
    token_response: Arc<Mutex<Value>>,
    token_requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    viewer_response: Arc<Mutex<Value>>,
}

async fn serve(provider: Provider) -> String {
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|State(p): State<Provider>, body: String| async move {
                let form: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                p.token_requests.lock().push(form);
                Json(p.token_response.lock().clone())
            }),
        )
        .route(
            "/graphql",
            post(|State(p): State<Provider>| async move { Json(p.viewer_response.lock().clone()) }),
        )
        .route(
            "/whoami",
            get(|State(p): State<Provider>| async move { Json(p.viewer_response.lock().clone()) }),
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
        "data": {"viewer": {"id": "app-user-1", "name": "Engrams Agent"}}
    });
    let host = serve(provider.clone()).await;

    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(1060));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let secrets = InMemorySecretStore::with_secrets([
        ("linear.client_id", "client-123"),
        ("linear.client_secret", "secret-456"),
    ]);
    let manager = OAuthManager::new(
        meta.clone(),
        Arc::new(EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1")),
        clock.clone(),
        entropy,
        Arc::new(secrets),
    );
    Fixture {
        manager,
        meta,
        clock,
        kek: EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1"),
        host,
        provider,
    }
}

fn linear_key() -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-default".into(),
        provider: "linear".into(),
    }
}

fn spec(host: &str, probe: bool) -> RedirectOauthSpec {
    RedirectOauthSpec {
        authorize_url: "https://linear.app/oauth/authorize".into(),
        token_url: format!("http://{host}/oauth/token"),
        scopes: vec!["read".into(), "write".into()],
        scope_delimiter: String::new(),
        extra_authorize_params: BTreeMap::from([("actor".to_string(), "app".to_string())]),
        client_id_ref: "linear.client_id".into(),
        client_secret_ref: "linear.client_secret".into(),
        pkce: false,
        metadata: RedirectMetadataSpec {
            from_token_response: BTreeMap::new(),
            probe: probe.then(|| RedirectMetadataProbe {
                method: "POST".into(),
                host: host.to_string(),
                path: "/graphql".into(),
                body: r#"{"query":"{ viewer { id name } }"}"#.into(),
                map: BTreeMap::from([
                    (META_ACCOUNT_ID.to_string(), "data.viewer.id".to_string()),
                    (
                        META_DISPLAY_NAME.to_string(),
                        "data.viewer.name".to_string(),
                    ),
                ]),
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

#[tokio::test]
async fn begin_builds_authorize_url_and_flow_row() {
    let fx = fixture().await;
    let begun = fx
        .manager
        .begin_redirect(
            linear_key(),
            &spec(&fx.host, false),
            "https://app.example.com/api/v1/integrations/linear/oauth/callback",
        )
        .await
        .expect("begin");

    let url = reqwest::Url::parse(&begun.authorize_url).expect("authorize url");
    let q: BTreeMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(url.host_str(), Some("linear.app"));
    assert_eq!(q["client_id"], "client-123");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["scope"], "read,write");
    assert_eq!(q["actor"], "app");
    assert_eq!(q["state"], begun.flow.id.to_string());
    assert!(!q.contains_key("code_challenge"));

    let flow = fx
        .meta
        .get_oauth_flow(begun.flow.id)
        .await
        .expect("get flow")
        .expect("flow row");
    assert_eq!(flow.status, OAuthFlowStatus::Pending);
    assert_eq!(flow.key, linear_key());
}

#[tokio::test]
async fn second_begin_supersedes_a_stale_pending_attempt() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    let first = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("first begin");
    let second = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("second begin supersedes");
    assert_ne!(first.flow.id, second.flow.id);
    let stale = fx
        .meta
        .get_oauth_flow(first.flow.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(stale.status, OAuthFlowStatus::Cancelled);
    assert_eq!(stale.error_code.as_deref(), Some("superseded"));
}

#[tokio::test]
async fn complete_publishes_bundle_with_rotation_and_probe_metadata() {
    let fx = fixture().await;
    let sp = spec(&fx.host, true);
    let begun = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let row = fx
        .manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "code-abc",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect("complete");

    // The provider saw a well-formed exchange.
    let reqs = fx.provider.token_requests.lock().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["grant_type"], "authorization_code");
    assert_eq!(reqs[0]["code"], "code-abc");
    assert_eq!(reqs[0]["client_id"], "client-123");
    assert_eq!(reqs[0]["client_secret"], "secret-456");
    assert_eq!(reqs[0]["redirect_uri"], "https://cb.example.com/x");

    // Probe metadata extracted; expiry mirrored outside the ciphertext.
    assert_eq!(row.metadata.account_id, "app-user-1");
    assert_eq!(row.metadata.display_name.as_deref(), Some("Engrams Agent"));
    let expires = row.expires_at.expect("expires_at");
    assert_eq!(
        expires,
        fx.clock.now_utc() + chrono::Duration::seconds(86400)
    );

    // The sealed bundle kept BOTH tokens and the embedded refresh spec.
    let bundle = open_bundle(&fx, &linear_key()).await;
    assert_eq!(bundle.access_token, "at-1");
    assert_eq!(bundle.refresh_token.as_deref(), Some("rt-1"));
    assert_eq!(bundle.scope.as_deref(), Some("read,write"));
    assert!(bundle.refreshable());
    let refresh = bundle.refresh.expect("refresh spec");
    assert_eq!(refresh.token_url, sp.token_url);
    assert_eq!(refresh.client_id_ref, "linear.client_id");

    let flow = fx
        .meta
        .get_oauth_flow(begun.flow.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(flow.status, OAuthFlowStatus::Succeeded);
}

#[tokio::test]
async fn slack_ok_false_body_fails_the_flow() {
    let fx = fixture().await;
    *fx.provider.token_response.lock() = json!({"ok": false, "error": "invalid_code"});
    let sp = spec(&fx.host, false);
    let begun = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let err = fx
        .manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "bad-code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect_err("must fail");
    assert!(format!("{err:?}").contains("invalid_code"), "{err:?}");
    let flow = fx
        .meta
        .get_oauth_flow(begun.flow.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(flow.status, OAuthFlowStatus::Failed);
    assert!(
        fx.meta
            .get_oauth_credential(&linear_key())
            .await
            .expect("get")
            .is_none(),
        "no credential row on a failed exchange"
    );
}

#[tokio::test]
async fn array_scope_and_no_refresh_token_normalize() {
    let fx = fixture().await;
    *fx.provider.token_response.lock() = json!({
        "access_token": "xoxb-slack",
        "token_type": "bearer",
        "scope": ["chat:write", "channels:read"],
        "team": {"id": "T123", "name": "Acme"},
    });
    let mut sp = spec(&fx.host, false);
    sp.metadata.from_token_response = BTreeMap::from([
        (META_WORKSPACE_ID.to_string(), "team.id".to_string()),
        (META_WORKSPACE_NAME.to_string(), "team.name".to_string()),
        (META_ACCOUNT_ID.to_string(), "team.id".to_string()),
    ]);
    let begun = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let row = fx
        .manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect("complete");
    assert_eq!(row.metadata.workspace_id.as_deref(), Some("T123"));
    assert_eq!(row.metadata.workspace_name.as_deref(), Some("Acme"));
    assert!(row.expires_at.is_none(), "non-expiring provider");

    let bundle = open_bundle(&fx, &linear_key()).await;
    assert_eq!(bundle.scope.as_deref(), Some("chat:write,channels:read"));
    assert!(bundle.refresh_token.is_none());
    assert!(bundle.refresh.is_none());
    assert!(!bundle.refreshable());
}

#[tokio::test]
async fn pkce_challenge_rides_begin_and_verifier_rides_complete() {
    let fx = fixture().await;
    let mut sp = spec(&fx.host, false);
    sp.pkce = true;
    let begun = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let url = reqwest::Url::parse(&begun.authorize_url).expect("url");
    let q: BTreeMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(q["code_challenge_method"], "S256");
    let challenge = q["code_challenge"].clone();

    fx.manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect("complete");
    let reqs = fx.provider.token_requests.lock().clone();
    let verifier = reqs[0]["code_verifier"].clone();
    // The derived verifier satisfies S256(verifier) == the begin challenge —
    // recomputed on "another replica" with nothing persisted.
    use base64::Engine as _;
    use sha2::Digest as _;
    let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(recomputed, challenge);
}

#[tokio::test]
async fn wrong_state_expired_flow_and_subject_mismatch_are_rejected() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);

    // Unknown state.
    let missing = fx
        .manager
        .complete_redirect(
            linear_key(),
            uuid::Uuid::from_u128(42),
            "code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect_err("unknown flow");
    assert!(format!("{missing:?}").contains("NotFound"), "{missing:?}");

    // Subject mismatch: a flow begun for one subject cannot be completed by
    // another.
    let begun = fx
        .manager
        .begin_redirect(linear_key(), &sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    let other = OAuthCredentialKey {
        subject_id: "conn-other".into(),
        ..linear_key()
    };
    let mismatch = fx
        .manager
        .complete_redirect(
            other,
            begun.flow.id,
            "code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect_err("subject mismatch");
    assert!(format!("{mismatch:?}").contains("NotFound"), "{mismatch:?}");

    // Expired flow.
    fx.clock.advance(std::time::Duration::from_secs(16 * 60));
    let expired = fx
        .manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "code",
            "https://cb.example.com/x",
            &sp,
        )
        .await
        .expect_err("expired flow");
    assert!(format!("{expired:?}").contains("expired"), "{expired:?}");
    assert!(
        fx.provider.token_requests.lock().is_empty(),
        "no provider call was made for any rejected completion"
    );
}

// ---------------------------------------------------------------------------
// Refresh machinery (scanner + resolve_connector_token) over the same fake
// provider.
// ---------------------------------------------------------------------------

/// Connect, then hand back the flow-completion row for refresh tests.
async fn connected(fx: &Fixture, sp: &RedirectOauthSpec) {
    let begun = fx
        .manager
        .begin_redirect(linear_key(), sp, "https://cb.example.com/x")
        .await
        .expect("begin");
    fx.manager
        .complete_redirect(
            linear_key(),
            begun.flow.id,
            "code-abc",
            "https://cb.example.com/x",
            sp,
        )
        .await
        .expect("complete");
    fx.provider.token_requests.lock().clear();
}

#[tokio::test]
async fn scanner_refreshes_ahead_of_expiry_and_adopts_rotation() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    // Outside the 6h margin of a 24h token: the sweep sees nothing due.
    fx.clock.advance(std::time::Duration::from_secs(3600));
    let idle = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(idle.examined, 0, "23h of validity left; nothing due");

    // Inside the margin: the sweep refreshes and adopts the ROTATED pair.
    *fx.provider.token_response.lock() = json!({
        "access_token": "at-2",
        "token_type": "Bearer",
        "expires_in": 86400,
        "refresh_token": "rt-2",
    });
    fx.clock.advance(std::time::Duration::from_secs(18 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(sweep.refreshed, 1, "{sweep:?}");

    let reqs = fx.provider.token_requests.lock().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["grant_type"], "refresh_token");
    assert_eq!(reqs[0]["refresh_token"], "rt-1");
    assert_eq!(reqs[0]["client_id"], "client-123");

    let bundle = open_bundle(&fx, &linear_key()).await;
    assert_eq!(bundle.access_token, "at-2");
    assert_eq!(bundle.refresh_token.as_deref(), Some("rt-2"));
    assert_eq!(
        bundle.expires_at,
        Some(fx.clock.now_utc() + chrono::Duration::seconds(86400))
    );
    // The mirrored column moved with the bundle, so the next sweep is idle.
    let again = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(again.examined, 0, "{again:?}");
}

#[tokio::test]
async fn resolve_is_readonly_outside_margin_and_refreshes_inline_inside() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    let fresh = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect("resolve");
    assert_eq!(fresh.secret, "at-1");
    assert!(
        fx.provider.token_requests.lock().is_empty(),
        "no refresh outside the margin"
    );

    // A provider that does NOT rotate keeps the old refresh token.
    *fx.provider.token_response.lock() = json!({
        "access_token": "at-2",
        "token_type": "Bearer",
        "expires_in": 86400,
    });
    fx.clock.advance(std::time::Duration::from_secs(19 * 3600));
    let refreshed = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect("resolve refreshes inline");
    assert_eq!(refreshed.secret, "at-2");
    assert_eq!(
        fx.provider.token_requests.lock().len(),
        1,
        "exactly one inline refresh"
    );
    let bundle = open_bundle(&fx, &linear_key()).await;
    assert_eq!(
        bundle.refresh_token.as_deref(),
        Some("rt-1"),
        "non-rotating provider keeps the prior refresh token"
    );
}

#[tokio::test]
async fn resolve_serves_the_stale_token_on_transient_refresh_failure() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    // A 200 with no access_token and no invalid_grant is a transient
    // exchange failure (provider hiccup shape).
    *fx.provider.token_response.lock() = json!({"error": "temporarily_unavailable"});
    fx.clock.advance(std::time::Duration::from_secs(19 * 3600));
    let stale = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect("stale token, not an error");
    assert_eq!(stale.secret, "at-1");

    let row = fx
        .meta
        .get_oauth_credential(&linear_key())
        .await
        .expect("get")
        .expect("row");
    assert!(
        row.broken_at.is_none(),
        "transient failure never marks broken"
    );
}

#[tokio::test]
async fn invalid_grant_breaks_the_credential_and_reconnect_repairs_it() {
    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    *fx.provider.token_response.lock() = json!({"error": "invalid_grant"});
    fx.clock.advance(std::time::Duration::from_secs(19 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(sweep.broke, 1, "{sweep:?}");

    let row = fx
        .meta
        .get_oauth_credential(&linear_key())
        .await
        .expect("get")
        .expect("row");
    assert!(row.broken_at.is_some());
    assert_eq!(row.broken_reason.as_deref(), Some("invalid_grant"));
    let err = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect_err("broken credential does not resolve");
    assert!(format!("{err:?}").contains("Disconnected"), "{err:?}");
    // Broken rows leave the sweep entirely.
    let idle = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(idle.examined, 0);

    // Reconnect through a fresh flow clears the mark and resolves again.
    *fx.provider.token_response.lock() = json!({
        "access_token": "at-3",
        "token_type": "Bearer",
        "expires_in": 86400,
        "refresh_token": "rt-3",
    });
    connected(&fx, &sp).await;
    let resolved = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect("repaired");
    assert_eq!(resolved.secret, "at-3");
}

#[tokio::test]
async fn non_refreshable_expiring_token_breaks_at_expiry_not_before() {
    let fx = fixture().await;
    // A provider shape neither shipped connector produces but a custom
    // connector can: an expiring access token with NO refresh token.
    *fx.provider.token_response.lock() = json!({
        "access_token": "at-noref",
        "token_type": "Bearer",
        "expires_in": 86400,
    });
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    // Inside the margin but still valid: the sweep leaves it alone (skip,
    // claim-bounded) and resolution still serves the working token.
    fx.clock.advance(std::time::Duration::from_secs(19 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!((sweep.skipped, sweep.broke), (1, 0), "{sweep:?}");
    let resolved = fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .expect("still valid");
    assert_eq!(resolved.secret, "at-noref");

    // Past expiry there is no repair path: the row breaks, leaves the due
    // set, resolution stops serving the dead token, and the status maps to
    // needs-reconnect instead of a "connected" lie.
    fx.clock.advance(std::time::Duration::from_secs(6 * 3600));
    let sweep = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(sweep.broke, 1, "{sweep:?}");
    let row = fx
        .meta
        .get_oauth_credential(&linear_key())
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.broken_reason.as_deref(), Some("no_refresh_token"));
    assert!(fx
        .manager
        .resolve_connector_token(&linear_key())
        .await
        .is_err());
    let idle = fx
        .manager
        .run_connector_refresh_once()
        .await
        .expect("sweep");
    assert_eq!(idle.examined, 0, "broken rows leave the due set: {idle:?}");
}

#[tokio::test]
async fn an_unsealable_row_does_not_abort_the_sweep() {
    use engram_core::types::oauth::{NewSealedOAuthCredential, OAuthAccountMetadata};

    let fx = fixture().await;
    let sp = spec(&fx.host, false);
    connected(&fx, &sp).await;

    // A second connector row whose ciphertext cannot be opened, with the
    // EARLIEST expiry so the ascending due order visits it first.
    let corrupt_key = OAuthCredentialKey {
        provider: "corruptco".into(),
        ..linear_key()
    };
    fx.meta
        .put_oauth_credential(
            NewSealedOAuthCredential {
                key: corrupt_key.clone(),
                wrapped_dek: vec![1, 2, 3],
                nonce: vec![0; 12],
                ciphertext: vec![4, 5, 6],
                key_id: "test:v1".into(),
                metadata: OAuthAccountMetadata {
                    account_id: "acct-corrupt".into(),
                    display_name: None,
                    plan_type: None,
                    workspace_id: None,
                    workspace_name: None,
                },
                expires_at: Some(fx.clock.now_utc() - chrono::Duration::hours(1)),
            },
            None,
        )
        .await
        .expect("seed corrupt row");

    // The healthy row is inside its margin; the corrupt row is due first.
    // The sweep must skip the corrupt row and still refresh the healthy one.
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
        .expect("one bad row must not abort the batch");
    assert_eq!((sweep.skipped, sweep.refreshed), (1, 1), "{sweep:?}");
    let bundle = open_bundle(&fx, &linear_key()).await;
    assert_eq!(bundle.access_token, "at-2");
}
