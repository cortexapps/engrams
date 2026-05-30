//! GitHub App forge tests against a mocked GitHub API (wiremock). The
//! JWT is signed with a throwaway RSA key; wiremock does not verify the
//! signature, so these exercise our request/response handling + caching.

use std::sync::OnceLock;

use engram_core::error::GitForgeError;
use engram_core::traits::{GitForge, PullRequestSpec, RepoRef};
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

fn repo() -> RepoRef {
    RepoRef::parse("cortexapps/engrams").unwrap()
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

    let t1 = app
        .mint_installation_token(Some("cortexapps"))
        .await
        .unwrap();
    assert_eq!(t1.username, "x-access-token");
    assert_eq!(t1.password, "ghs_abc123");

    let t2 = app
        .mint_installation_token(Some("cortexapps"))
        .await
        .unwrap();
    assert_eq!(t2.password, "ghs_abc123");
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
    let spec = PullRequestSpec {
        head_branch: "feat/x".into(),
        base_branch: "main".into(),
        title: "Add x".into(),
        body: "does x".into(),
        draft: false,
    };
    let pr = app.create_pull_request(&repo(), &spec).await.unwrap();
    assert_eq!(pr.url, "https://github.com/cortexapps/engrams/pull/7");
    assert_eq!(pr.id, 7);
    assert_eq!(pr.state, "open");
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
    let spec = PullRequestSpec {
        head_branch: "feat/x".into(),
        base_branch: "main".into(),
        title: "dup".into(),
        body: String::new(),
        draft: false,
    };
    let err = app.create_pull_request(&repo(), &spec).await.unwrap_err();
    assert!(
        matches!(err, GitForgeError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
}

#[tokio::test]
async fn rejects_invalid_private_key() {
    // `GitHubApp` holds an `EncodingKey` (not `Debug`), so match the
    // `Result` directly rather than `unwrap_err()`.
    assert!(matches!(
        GitHubApp::new("123", "not a pem"),
        Err(GitForgeError::Unauthorized(_))
    ));
}
