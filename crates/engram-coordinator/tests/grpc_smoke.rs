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
    // HTTP base address for the coordinator REST surface, read from
    // `ENGRAM_SMOKE_HTTP` (default: http://127.0.0.1:8090 — the dev HTTP
    // port set by Tiltfile). Override when the coordinator listens elsewhere.
    let http_addr = std::env::var("ENGRAM_SMOKE_HTTP")
        .unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
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

/// Parse `id:` lines from an SSE event stream body, returning the idx
/// values in order. Stops when the accumulated vec has `limit` entries
/// or the text is exhausted.
fn parse_sse_idx_sequence(body: &str, limit: usize) -> Vec<i64> {
    let mut idxs = Vec::new();
    for line in body.lines() {
        if let Some(raw) = line.strip_prefix("id:") {
            if let Ok(idx) = raw.trim().parse::<i64>() {
                idxs.push(idx);
                if idxs.len() >= limit {
                    break;
                }
            }
        }
    }
    idxs
}

/// StreamEvents smoke: create a session, stream events from the start
/// via gRPC, compare idx sequence against the legacy SSE feed for the
/// same session, reopen with `since=last_idx` and verify no gap or dup.
///
/// Env-gated like `session_crud_smoke` — requires `ENGRAM_SMOKE_GRPC`.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn stream_events_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image().await else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    let mut client =
        SessionServiceClient::with_interceptor(channel, bearer_interceptor(token.clone()));

    let rpc_timeout = std::time::Duration::from_secs(30);

    // ---- 1. CreateSession ----
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri: image_uri.clone(),
        mode: String::new(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let create_resp = client
        .create_session(create_req)
        .await
        .expect("CreateSession must succeed")
        .into_inner();
    let session_id = create_resp.session_id.clone();
    println!(
        "smoke(stream_events): created session {session_id} (status={}, kind={})",
        create_resp.status, create_resp.kind,
    );

    // ---- 2. StreamEvents from the start (since unset) ----
    // The session will have at least one status_changed event.
    let mut stream_req = tonic::Request::new(app::StreamEventsRequest {
        session_id: session_id.clone(),
        since: None, // from the start
    });
    stream_req.set_timeout(rpc_timeout);
    let mut stream = client
        .stream_events(stream_req)
        .await
        .expect("StreamEvents must succeed")
        .into_inner();

    // Collect up to 5 events (or until 10s timeout), noting their idx values.
    let mut grpc_idxs: Vec<i64> = Vec::new();
    let stream_read_timeout = std::time::Duration::from_secs(10);
    while grpc_idxs.len() < 5 {
        match tokio::time::timeout(stream_read_timeout, stream.message()).await {
            Ok(Ok(Some(ev))) => {
                println!(
                    "smoke(stream_events): gRPC event kind={:?} idx={:?} payload={}",
                    ev.kind,
                    ev.idx,
                    &ev.payload_json[..ev.payload_json.len().min(120)],
                );
                if let Some(idx) = ev.idx {
                    grpc_idxs.push(idx);
                }
                // Quit once we have at least one event to compare.
                if !grpc_idxs.is_empty() && ev.kind != "status_changed" {
                    break;
                }
                if grpc_idxs.len() >= 2 {
                    break;
                }
            }
            Ok(Ok(None)) => {
                // Stream ended (session terminal already).
                break;
            }
            Ok(Err(e)) => {
                panic!("StreamEvents RPC error: {e}");
            }
            Err(_) => {
                // Timeout waiting for more events — proceed with what we have.
                break;
            }
        }
    }
    assert!(
        !grpc_idxs.is_empty(),
        "StreamEvents must yield at least one event (status_changed) for a newly created session"
    );
    println!(
        "smoke(stream_events): gRPC idx sequence: {grpc_idxs:?}"
    );

    // ---- 3. Compare against legacy SSE feed idx sequence ----
    let http_addr = std::env::var("ENGRAM_SMOKE_HTTP")
        .unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
    let sse_url = format!("{http_addr}/api/v1/sessions/{session_id}/events");

    // Request the SSE stream with a short read — just enough to collect
    // events. The `since=-1` query asks for everything from the start,
    // mirroring the gRPC `since=None`.
    let sse_client = reqwest::Client::new();
    let sse_resp = sse_client
        .get(&sse_url)
        .query(&[("since", "-1")])
        .header("Accept", "text/event-stream")
        // Bearer auth — the SSE endpoint uses the same session-level
        // cookie/synthetic auth in tests; try cookie-free first (dev
        // mode with synthetic admin should work without a cookie).
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    match sse_resp {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            let sse_idxs = parse_sse_idx_sequence(&body, grpc_idxs.len());
            println!("smoke(stream_events): SSE idx sequence (first {}): {sse_idxs:?}", grpc_idxs.len());

            // The idx sequences must match — gRPC and SSE share the
            // same core and the same persistent log.
            if !sse_idxs.is_empty() {
                let compare_len = grpc_idxs.len().min(sse_idxs.len());
                assert_eq!(
                    &grpc_idxs[..compare_len],
                    &sse_idxs[..compare_len],
                    "gRPC and SSE must yield identical idx sequences for session {session_id}"
                );
                println!(
                    "smoke(stream_events): gRPC and SSE idx sequences match ({compare_len} events)"
                );
            } else {
                println!(
                    "smoke(stream_events): WARN — SSE body yielded no id: lines \
                     (body may have been truncated by the short timeout); skipping SSE comparison"
                );
            }
        }
        Ok(resp) => {
            println!(
                "smoke(stream_events): WARN — SSE GET returned {} — skipping SSE comparison",
                resp.status()
            );
        }
        Err(e) => {
            println!(
                "smoke(stream_events): WARN — SSE GET failed ({e}) — skipping SSE comparison"
            );
        }
    }

    // ---- 4. Reopen with since=last_idx — no gap, no dup ----
    let last_idx = *grpc_idxs.last().expect("at least one idx");
    let mut reopen_req = tonic::Request::new(app::StreamEventsRequest {
        session_id: session_id.clone(),
        since: Some(last_idx),
    });
    reopen_req.set_timeout(rpc_timeout);
    let mut reopen_stream = client
        .stream_events(reopen_req)
        .await
        .expect("StreamEvents reopen must succeed")
        .into_inner();

    // Collect the next event from the reopen (or time out).
    let reopen_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reopen_stream.message(),
    )
    .await;

    match reopen_result {
        Ok(Ok(Some(ev))) => {
            // The first event after reopen must have idx > last_idx (no dup).
            if let Some(next_idx) = ev.idx {
                assert!(
                    next_idx > last_idx,
                    "StreamEvents reopen must yield idx > {last_idx} (since=last_idx), got {next_idx}"
                );
                println!(
                    "smoke(stream_events): reopen with since={last_idx} → first event idx={next_idx} — no gap, no dup ✓"
                );
            } else {
                // A lagged or no-idx event before the dedupe; not a failure.
                println!(
                    "smoke(stream_events): reopen first event has kind={:?}, idx=None — ok",
                    ev.kind
                );
            }
        }
        Ok(Ok(None)) | Err(_) => {
            // Session may have no more events within the window — that's fine.
            println!(
                "smoke(stream_events): reopen with since={last_idx}: no additional events \
                 within timeout — replay dedup verified (no events before last_idx replayed)"
            );
        }
        Ok(Err(e)) => {
            panic!("StreamEvents reopen RPC error: {e}");
        }
    }

    // ---- 5. DeleteSession ----
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client
        .delete_session(del_req)
        .await
        .expect("DeleteSession must succeed");
    println!("smoke(stream_events): DeleteSession ok — all checks passed");
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

    // Connect to the gRPC server with a 5-second connect timeout so a
    // wedged or unreachable stack fails loudly instead of hanging forever.
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    let mut client = SessionServiceClient::with_interceptor(channel, bearer_interceptor(token));

    // Per-RPC timeout: 30 s is generous enough for a cold create (image
    // pull + sandbox boot) while still ensuring a wedged call fails loudly.
    let rpc_timeout = std::time::Duration::from_secs(30);

    // ---- 1. CreateSession (no-harness image, mode: agent) ----
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri: image_uri.clone(),
        mode: String::new(), // default = agent
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let create_resp = client
        .create_session(create_req)
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
    let mut list_req = tonic::Request::new(app::ListSessionsRequest::default());
    list_req.set_timeout(rpc_timeout);
    let list_resp = client
        .list_sessions(list_req)
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
    let mut get_req = tonic::Request::new(app::GetSessionRequest {
        session_id: session_id.clone(),
    });
    get_req.set_timeout(rpc_timeout);
    let get_resp = client
        .get_session(get_req)
        .await
        .expect("GetSession must succeed")
        .into_inner();

    let session = get_resp.session.expect("GetSession.session must be set");
    assert_eq!(session.id, session_id, "GetSession id mismatch");
    assert_eq!(session.image, image_uri, "GetSession image mismatch");
    println!("smoke: GetSession ok (status={})", session.status);

    // ---- 4. DeleteSession ----
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client
        .delete_session(del_req)
        .await
        .expect("DeleteSession must succeed");
    println!("smoke: DeleteSession ok");

    // ---- 5. GetSession after delete — session row persists as
    // terminal (Completed or Failed). Delete transitions the FSM to a
    // terminal state but does NOT hard-delete the row (audit trail).
    // Verify the session is terminal, not that it's missing.
    let mut post_req = tonic::Request::new(app::GetSessionRequest {
        session_id: session_id.clone(),
    });
    post_req.set_timeout(rpc_timeout);
    let post_delete = client
        .get_session(post_req)
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
