//! GitHub App forge tests against a mocked GitHub API (wiremock). The
//! JWT is signed with a throwaway RSA key; wiremock does not verify the
//! signature, so these exercise our request/response handling + caching.

use std::sync::OnceLock;

use engram_core::error::IntegrationError;
use engram_core::traits::{CredentialHint, Integration, ScopedCredential};
use engram_git_github::GitHubApp;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A throwaway 2048-bit RSA key, generated once per test binary. The JWT
/// it signs is sent to wiremock, which doesn't verify the signature — we
/// only need a key `jsonwebtoken` accepts. Generated rather than
/// committed so no PEM material lands in git history.
fn test_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).expect("generate test RSA key");
        key.to_pkcs8_pem(LineEnding::LF)
            .expect("encode pkcs8 pem")
            .to_string()
    })
}

async fn mount_installation(server: &MockServer, times: u64) {
    Mock::given(method("GET"))
        .and(path("/orgs/cortexapps/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": 42 })))
        .expect(times)
        .mount(server)
        .await;
}

async fn mount_token(server: &MockServer, times: u64) {
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_abc123",
            "expires_at": "2099-01-01T00:00:00Z",
        })))
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test]
async fn mints_and_caches_installation_token() {
    let server = MockServer::start().await;
    // Installation looked up once, token minted once — the second call
    // is served from cache (both `.expect(1)` verified on drop).
    mount_installation(&server, 1).await;
    mount_token(&server, 1).await;

    let app = GitHubApp::new("123", test_key())
        .unwrap()
        .with_base_url(server.uri());

    let hint = CredentialHint {
        served_host: None,
        owner: Some("cortexapps".into()),
    };
    let t1 = app.mint_credential(&[], &hint).await.unwrap();
    let ScopedCredential::Basic {
        username, password, ..
    } = &t1
    else {
        panic!("expected a basic credential, got {t1:?}");
    };
    assert_eq!(username, "x-access-token");
    assert_eq!(password, "ghs_abc123");

    // Same scope (empty caps + same owner) → served from cache (both mocks
    // verified `.expect(1)` on drop).
    let t2 = app.mint_credential(&[], &hint).await.unwrap();
    assert!(matches!(t2, ScopedCredential::Basic { password, .. } if password == "ghs_abc123"));
}

#[tokio::test]
async fn mint_rejects_a_served_host_it_does_not_own() {
    // served_host is our git host (github.com), NOT the API endpoint
    // (api.github.com). Handing the connector's egress/API host is a namespace
    // mismatch and is refused before any HTTP — the false negative a connector
    // "test connection" hit before the mint hint was fixed.
    let app = GitHubApp::new("123", test_key()).unwrap();
    let hint = CredentialHint {
        served_host: Some("api.github.com".into()),
        owner: None,
    };
    let err = app.mint_credential(&[], &hint).await.unwrap_err();
    assert!(
        err.to_string().contains("does not serve host"),
        "expected a served-host rejection, got: {err}"
    );
}

#[tokio::test]
async fn rejects_invalid_private_key() {
    // `GitHubApp` holds an `EncodingKey` (not `Debug`), so match the
    // `Result` directly rather than `unwrap_err()`.
    assert!(matches!(
        GitHubApp::new("123", "not a pem"),
        Err(IntegrationError::Unauthorized(_))
    ));
}

// --- ADR 0057 C2: the github_app mint-kind descriptor ----------------------

#[test]
fn github_app_descriptor_metadata() {
    use engram_core::traits::MintFieldKind;
    let d = engram_git_github::github_app_descriptor();
    assert_eq!(d.kind, "github_app");
    assert_eq!(d.provider, "github");
    // The field names are the org-secret suffixes the coordinator resolves; the
    // Plane-A form (C4) renders from this metadata, so pin the contract.
    let names: Vec<&str> = d.fields.iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["app_id", "private_key_pem"]);
    let pem = d
        .fields
        .iter()
        .find(|f| f.name == "private_key_pem")
        .unwrap();
    assert_eq!(pem.field_kind, MintFieldKind::SealedSecret);
    assert!(pem.required);
    let app_id = d.fields.iter().find(|f| f.name == "app_id").unwrap();
    assert_eq!(app_id.field_kind, MintFieldKind::Config);
}

#[test]
fn github_app_descriptor_builds_engine_from_resolved_fields() {
    use engram_core::traits::ResolvedFields;
    let mut fields = ResolvedFields::new();
    fields.insert("app_id".into(), "123".into());
    fields.insert("private_key_pem".into(), test_key().to_string());
    let engine = (engram_git_github::github_app_descriptor().build)(&fields).expect("build engine");
    assert_eq!(engine.provider(), "github");
}

#[test]
fn github_app_descriptor_rejects_missing_or_bad_fields() {
    use engram_core::traits::ResolvedFields;
    let d = engram_git_github::github_app_descriptor();
    // Missing private_key_pem → build error.
    let mut missing = ResolvedFields::new();
    missing.insert("app_id".into(), "123".into());
    assert!((d.build)(&missing).is_err());
    // Present but invalid PEM → build error.
    let mut bad = ResolvedFields::new();
    bad.insert("app_id".into(), "123".into());
    bad.insert("private_key_pem".into(), "not a pem".into());
    assert!((d.build)(&bad).is_err());
}
