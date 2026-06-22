//! GitHub App forge tests against a mocked GitHub API (wiremock). The
//! JWT is signed with a throwaway RSA key; wiremock does not verify the
//! signature, so these exercise our request/response handling + caching.

use std::sync::OnceLock;

use engram_core::error::IntegrationError;
use engram_core::traits::{CredentialHint, Integration, ScopedCredential};
use engram_core::types::Capability;
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

fn pulls_cap() -> Capability {
    Capability::parse("github:pulls:write").unwrap()
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
        host: None,
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
async fn creates_pull_request() {
    let server = MockServer::start().await;
    mount_installation(&server, 1).await;
    mount_token(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/repos/cortexapps/engrams/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "html_url": "https://github.com/cortexapps/engrams/pull/7",
            "number": 7,
            "state": "open",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let app = GitHubApp::new("123", test_key())
        .unwrap()
        .with_base_url(server.uri());
    let args = serde_json::json!({
        "repo": "cortexapps/engrams",
        "head_branch": "feat/x",
        "base_branch": "main",
        "title": "Add x",
        "body": "does x",
        "draft": false,
    });
    let reply = app.perform_action(&pulls_cap(), &args).await.unwrap();
    assert_eq!(reply["url"], "https://github.com/cortexapps/engrams/pull/7");
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["state"], "open");
}

#[tokio::test]
async fn maps_422_to_rejected() {
    let server = MockServer::start().await;
    mount_installation(&server, 1).await;
    mount_token(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/repos/cortexapps/engrams/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "A pull request already exists for cortexapps:feat/x.",
        })))
        .mount(&server)
        .await;

    let app = GitHubApp::new("123", test_key())
        .unwrap()
        .with_base_url(server.uri());
    let args = serde_json::json!({
        "repo": "cortexapps/engrams",
        "head_branch": "feat/x",
        "base_branch": "main",
        "title": "dup",
    });
    let err = app.perform_action(&pulls_cap(), &args).await.unwrap_err();
    assert!(
        matches!(err, IntegrationError::Rejected(_)),
        "expected Rejected, got {err:?}"
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
