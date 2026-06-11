//! Live smoke tests for the app-gRPC `SessionService` (ADR 0039, Task 10).
//!
//! Gate: `ENGRAM_SMOKE_GRPC` must be set to `<host>:<port>` (the
//! coordinator's `--app-grpc-addr`). When unset the test prints a skip
//! notice and returns `Ok` — the same graceful-skip idiom the live-pg
//! tests use (see `host_register_live_pg.rs`).
//!
//! Run against a running `just dev` stack:
//!
//! ```text
//! just smoke-control-plane
//! ```
//!
//! The Tiltfile configures the coordinator with bearer `dev-app-grpc-token`
//! (overridable via `ENGRAM_APP_GRPC_TOKENS`). The smoke reads enabled
//! images from the legacy REST `GET http://127.0.0.1:8090/api/v1/enabled-images`
//! to find a `harness_name: null` image — ImageService gRPC doesn't exist
//! until Task 13. It then drives create → list → get → delete over gRPC.

use engram_protocol::app;
use engram_protocol::app::session_service_client::SessionServiceClient;

/// Return value of the graceful-skip helper: either the gRPC address + bearer
/// from env, or `None` (print skipped, caller returns early).
fn grpc_addr_and_token() -> Option<(String, String)> {
    let addr = match std::env::var("ENGRAM_SMOKE_GRPC") {
        Ok(v) => v,
        Err(_) => {
            println!("SKIP: ENGRAM_SMOKE_GRPC not set — run `just dev` then `just smoke-control-plane`");
            return None;
        }
    };
    let token = std::env::var("ENGRAM_SMOKE_GRPC_TOKEN")
        .unwrap_or_else(|_| "dev-app-grpc-token".to_string());
    Some((addr, token))
}

/// `Authorization: Bearer <token>` interceptor.
// `result_large_err`: tonic requires `Result<_, Status>` for interceptors.
#[allow(clippy::result_large_err)]
fn bearer_interceptor(
    token: String,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .expect("bearer header is ASCII"),
        );
        Ok(req)
    }
}

/// Look up the REST `GET /api/v1/enabled-images` and pick the first image
/// whose `harness_name` is `null`. ImageService gRPC doesn't exist until
/// Task 13, so we fall back to the legacy REST surface.
///
/// Returns the image URI (e.g. `"localhost:5001/demo:warm"`) or `None` when
/// no no-harness image is enabled. The smoke prints a skip notice in that
/// case.
async fn find_no_harness_image() -> Option<String> {
    // Derive the HTTP addr from the smoke addr: use 8090 (the dev HTTP port).
    let http_addr = "http://127.0.0.1:8090";
    let url = format!("{http_addr}/api/v1/enabled-images");

    let resp = match reqwest::get(&url).await {
        Ok(r) => r,
        Err(e) => {
            println!("SKIP: GET {url} failed ({e}) — is `just dev` running?");
            return None;
        }
    };

    if !resp.status().is_success() {
        println!(
            "SKIP: GET {url} returned {} — coordinator may not be ready",
            resp.status()
        );
        return None;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            println!("SKIP: failed to parse enabled-images response: {e}");
            return None;
        }
    };

    let images = match body["images"].as_array() {
        Some(a) => a,
        None => {
            println!("SKIP: enabled-images response missing `images` array");
            return None;
        }
    };

    // Pick the first image whose `harness_name` is JSON null — the
    // coordinator creates these without a harness drive, so no harness
    // credential is needed for the gRPC create path.
    for img in images {
        if img["harness_name"].is_null() {
            if let Some(uri) = img["image_uri"].as_str() {
                println!("smoke: using no-harness image {uri:?}");
                return Some(uri.to_string());
            }
        }
    }

    println!(
        "SKIP: no enabled image with harness_name=null found. \
         Enable a no-harness image first (e.g. `just enable-image localhost:5001/demo:warm`)."
    );
    None
}

/// Create → list → get → delete end-to-end over gRPC.
///
/// `#[ignore]`d by default — runs only when `ENGRAM_SMOKE_GRPC` is set
/// (enforced by `just smoke-control-plane`).
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn session_crud_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image().await else {
        return;
    };

    // Connect to the gRPC server.
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint");
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    let mut client = SessionServiceClient::with_interceptor(channel, bearer_interceptor(token));

    // ---- 1. CreateSession (no-harness image, mode: agent) ----
    let create_resp = client
        .create_session(app::CreateSessionRequest {
            image_uri: image_uri.clone(),
            mode: String::new(), // default = agent
            prompt: None,
            secrets: std::collections::HashMap::new(),
            harness_secret_id: None,
        })
        .await
        .expect("CreateSession must succeed over gRPC")
        .into_inner();

    let session_id = create_resp.session_id.clone();
    println!(
        "smoke: created session {session_id} (status={}, kind={})",
        create_resp.status, create_resp.kind,
    );
    assert!(!session_id.is_empty(), "session_id must not be empty");

    // ---- 2. ListSessions — find our session ----
    let list_resp = client
        .list_sessions(app::ListSessionsRequest::default())
        .await
        .expect("ListSessions must succeed")
        .into_inner();

    let found = list_resp
        .sessions
        .iter()
        .any(|item| item.session.as_ref().map(|s| s.id.as_str()) == Some(session_id.as_str()));
    assert!(
        found,
        "newly created session {session_id} must appear in ListSessions; got {} rows",
        list_resp.sessions.len(),
    );
    println!("smoke: ListSessions returned {} rows, found our session", list_resp.sessions.len());

    // ---- 3. GetSession ----
    let get_resp = client
        .get_session(app::GetSessionRequest {
            session_id: session_id.clone(),
        })
        .await
        .expect("GetSession must succeed")
        .into_inner();

    let session = get_resp.session.expect("GetSession.session must be set");
    assert_eq!(session.id, session_id, "GetSession id mismatch");
    assert_eq!(session.image, image_uri, "GetSession image mismatch");
    println!("smoke: GetSession ok (status={})", session.status);

    // ---- 4. DeleteSession ----
    client
        .delete_session(app::DeleteSessionRequest {
            session_id: session_id.clone(),
        })
        .await
        .expect("DeleteSession must succeed");
    println!("smoke: DeleteSession ok");

    // ---- 5. GetSession after delete — session row persists as
    // terminal (Completed or Failed). Delete transitions the FSM to a
    // terminal state but does NOT hard-delete the row (audit trail).
    // Verify the session is terminal, not that it's missing.
    let post_delete = client
        .get_session(app::GetSessionRequest {
            session_id: session_id.clone(),
        })
        .await
        .expect("GetSession after delete must still return the row (terminal state, not gone)")
        .into_inner();
    let post_session = post_delete.session.expect("session must be set");
    let terminal = matches!(
        post_session.status.as_str(),
        "completed" | "failed" | "dead" | "host_lost"
    );
    assert!(
        terminal,
        "deleted session must be in a terminal status, got {:?}",
        post_session.status,
    );
    println!(
        "smoke: post-delete GetSession returned terminal status {:?} — all checks passed",
        post_session.status
    );
}
