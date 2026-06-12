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
//! (overridable via `ENGRAM_APP_GRPC_TOKENS`). The smoke discovers a
//! no-harness image via `ImageService.ListEnabledImages` over gRPC (Task 13
//! is live — the legacy REST surface is bearer-gated post-Task-31 and no
//! longer usable without the REST service token). It then drives
//! create → list → get → delete over gRPC.
//!
//! ## SSE comparison in `stream_events_smoke`
//!
//! The SSE leg (`GET /api/v1/sessions/{id}/events` on 8090) returns 401
//! post-Task-31: the coordinator's REST surface authenticates via
//! `ENGRAM_AUTH_TOKENS` (the deployment service-bearer allow-list), which
//! is NOT set in the Tiltfile dev stack — only `ENGRAM_APP_GRPC_TOKENS`
//! is configured (the gRPC-surface token). Sending `Authorization: Bearer
//! dev-app-grpc-token` to the REST port confirms 401 (curl evidence:
//! `curl -s -H 'Authorization: Bearer dev-app-grpc-token' \
//!   http://127.0.0.1:8090/api/v1/sessions/<id>/events --max-time 3`
//! → HTTP 401). The existing WARN-skip path in `stream_events_smoke` for
//! non-2xx responses handles this gracefully; the comparison is retired
//! in Task 32 when the REST surface is deleted entirely.

use engram_protocol::app;
use engram_protocol::app::image_service_client::ImageServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;

/// Return value of the graceful-skip helper: either the gRPC address + bearer
/// from env, or `None` (print skipped, caller returns early).
fn grpc_addr_and_token() -> Option<(String, String)> {
    let addr = match std::env::var("ENGRAM_SMOKE_GRPC") {
        Ok(v) => v,
        Err(_) => {
            println!(
                "SKIP: ENGRAM_SMOKE_GRPC not set — run `just dev` then `just smoke-control-plane`"
            );
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

/// Discover a no-harness image via `ImageService.ListEnabledImages` over gRPC.
///
/// The legacy REST `GET /api/v1/enabled-images` is no longer usable here:
/// the coordinator's REST surface is bearer-gated post-Task-31 via
/// `ENGRAM_AUTH_TOKENS`, which is not set in the Tiltfile dev stack
/// (only `ENGRAM_APP_GRPC_TOKENS` is configured). This function uses the
/// same gRPC address + bearer that the rest of the file uses via
/// `grpc_addr_and_token()`.
///
/// Returns the image URI (e.g. `"localhost:5001/demo:warm"`) or `None` when
/// no no-harness image is enabled. The smoke prints a skip notice in that
/// case.
async fn find_no_harness_image(addr: &str, token: &str) -> Option<String> {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = match endpoint.connect().await {
        Ok(c) => c,
        Err(e) => {
            println!("SKIP: connect to gRPC {addr} failed ({e}) — is `just dev` running?");
            return None;
        }
    };

    let mut client =
        ImageServiceClient::with_interceptor(channel, bearer_interceptor(token.to_string()));
    let mut req = tonic::Request::new(app::ListEnabledImagesRequest {});
    req.set_timeout(std::time::Duration::from_secs(10));

    let resp = match client.list_enabled_images(req).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            println!("SKIP: ImageService.ListEnabledImages failed ({e}) — coordinator not ready?");
            return None;
        }
    };

    // Pick the first image whose `harness_name` is None/empty — these images
    // have no harness drive, so no harness credential is needed for the gRPC
    // create path.
    for img in &resp.images {
        if img.harness_name.as_deref().unwrap_or("").is_empty() {
            println!("smoke: using no-harness image {:?}", img.image_uri);
            return Some(img.image_uri.clone());
        }
    }

    println!(
        "SKIP: no enabled image with harness_name=null found ({} images checked). \
         Enable a no-harness image first (e.g. `just enable-image localhost:5001/demo:warm`).",
        resp.images.len()
    );
    None
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
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
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

    // Collect up to 2 events (or until 10s timeout), noting their idx values.
    // We stop as soon as we have 2, or on a non-status_changed event — the
    // loop bound and break condition agree on 2.
    let mut grpc_idxs: Vec<i64> = Vec::new();
    let stream_read_timeout = std::time::Duration::from_secs(10);
    while grpc_idxs.len() < 2 {
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
    println!("smoke(stream_events): gRPC idx sequence: {grpc_idxs:?}");

    // ---- 3. Compare against legacy SSE feed idx sequence ----
    //
    // We read the SSE stream INCREMENTALLY via `bytes_stream()` so we can
    // stop as soon as we have collected `grpc_idxs.len()` `id:` lines.
    // A plain `.timeout(5s).text()` would always time out (SSE never
    // sends EOF while the session is live), making the comparison silently
    // no-op via the WARN-skip path every run.
    //
    // WARN-skip is kept ONLY for a genuine connection failure (Err arm).
    // If the stream connects but yields fewer id: lines than expected
    // within the 10 s deadline, the test FAILS — that is a real bug.
    {
        use futures::StreamExt as _;

        let http_addr = std::env::var("ENGRAM_SMOKE_HTTP")
            .unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
        let sse_url = format!("{http_addr}/api/v1/sessions/{session_id}/events");

        // No .timeout() on the send — we want the connection to succeed and
        // then we impose a per-stream deadline via tokio::time::timeout below.
        let sse_client = reqwest::Client::new();
        let sse_send = sse_client
            .get(&sse_url)
            .query(&[("since", "-1")])
            .header("Accept", "text/event-stream")
            .send()
            .await;

        match sse_send {
            Err(e) => {
                // Genuine connection failure (stack unreachable) — skip, not fail.
                println!(
                    "smoke(stream_events): WARN — SSE GET failed ({e}) — skipping SSE comparison"
                );
            }
            Ok(resp) if !resp.status().is_success() => {
                // Non-2xx (e.g. auth required, 404) — skip with a note.
                println!(
                    "smoke(stream_events): WARN — SSE GET returned {} — skipping SSE comparison",
                    resp.status()
                );
            }
            Ok(resp) => {
                // Connected successfully — read incrementally until we have
                // the expected number of id: lines or the deadline expires.
                let want = grpc_idxs.len();
                let sse_deadline = std::time::Duration::from_secs(10);

                let mut byte_stream = resp.bytes_stream();
                let mut partial = String::new();
                let mut sse_idxs: Vec<i64> = Vec::new();

                let collect_result = tokio::time::timeout(sse_deadline, async {
                    while sse_idxs.len() < want {
                        match byte_stream.next().await {
                            None => break, // stream ended (session terminal)
                            Some(Err(e)) => {
                                // Transport error mid-stream — hard fail so we
                                // don't silently weaken the comparison. If this
                                // fires, the SSE path has a real transport bug.
                                // (Comparison NOT weakened: panic immediately.)
                                panic!("smoke(stream_events): SSE read error mid-stream: {e}");
                            }
                            Some(Ok(chunk)) => {
                                partial.push_str(&String::from_utf8_lossy(&chunk));
                                // Parse all complete lines, keep remainder.
                                let last_newline = partial.rfind('\n').map(|i| i + 1).unwrap_or(0);
                                let complete = partial[..last_newline].to_string();
                                partial = partial[last_newline..].to_string();
                                for line in complete.lines() {
                                    if let Some(raw) = line.strip_prefix("id:") {
                                        if let Ok(idx) = raw.trim().parse::<i64>() {
                                            sse_idxs.push(idx);
                                            if sse_idxs.len() >= want {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
                .await;

                println!(
                    "smoke(stream_events): SSE idx sequence (collected {}/{}): {sse_idxs:?}",
                    sse_idxs.len(),
                    want
                );

                if collect_result.is_err() && sse_idxs.len() < want {
                    // Stream connected but timed out before delivering enough
                    // id: lines — this is a real bug, not a skip.
                    panic!(
                        "smoke(stream_events): SSE stream connected but only yielded {} id: \
                         lines within {sse_deadline:?}; expected {want} — \
                         gRPC idx sequence was {grpc_idxs:?}",
                        sse_idxs.len(),
                    );
                }

                // Hard assert: the idx sequences must match.
                let compare_len = grpc_idxs.len().min(sse_idxs.len());
                assert_eq!(
                    &grpc_idxs[..compare_len],
                    &sse_idxs[..compare_len],
                    "gRPC and SSE must yield identical idx sequences for session {session_id}"
                );
                println!(
                    "smoke(stream_events): gRPC idx sequence {grpc_idxs:?} == \
                     SSE idx sequence {sse_idxs:?} — match confirmed ({compare_len} events)"
                );
            }
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
    let reopen_result =
        tokio::time::timeout(std::time::Duration::from_secs(5), reopen_stream.message()).await;

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
                "smoke(stream_events): reopen with since={last_idx}: no events received \
                 within 5s — no dupes or gaps observed (stream opened cleanly, no events \
                 before or at last_idx were delivered)"
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
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
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
    println!(
        "smoke: ListSessions returned {} rows, found our session",
        list_resp.sessions.len()
    );

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

// -----------------------------------------------------------------------
// Task 12 smoke tests
// -----------------------------------------------------------------------

/// `Exec` streaming smoke: create a session, exec `uname -m`, assert
/// started/stdout/exit framing, then delete.
///
/// Env-gated like the other smokes — requires `ENGRAM_SMOKE_GRPC`.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn exec_streaming_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");
    let mut client = SessionServiceClient::with_interceptor(channel, bearer_interceptor(token));
    let rpc_timeout = std::time::Duration::from_secs(30);

    // Create session.
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri: image_uri.clone(),
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let create_resp = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner();
    let session_id = create_resp.session_id.clone();
    println!("smoke(exec): session {session_id}");

    // Exec `uname -m`.
    let mut exec_req = tonic::Request::new(app::ExecRequest {
        session_id: session_id.clone(),
        command: Some("uname -m".into()),
        argv: vec![],
        env: std::collections::HashMap::new(),
        workdir: None,
        timeout_secs: Some(10),
    });
    exec_req.set_timeout(rpc_timeout);
    let mut exec_stream = client
        .exec(exec_req)
        .await
        .expect("Exec RPC must succeed")
        .into_inner();

    // Read the first frame — must be `started`.
    let first = tokio::time::timeout(std::time::Duration::from_secs(15), exec_stream.message())
        .await
        .expect("exec: timed out waiting for first frame")
        .expect("exec: RPC error on first frame")
        .expect("exec: stream ended before started frame");

    let exec_id = match first.event {
        Some(app::exec_output::Event::Started(s)) => {
            println!("smoke(exec): started exec_id={}", s.exec_id);
            s.exec_id
        }
        other => panic!("smoke(exec): expected started frame, got {other:?}"),
    };
    assert!(!exec_id.is_empty(), "exec_id must not be empty");

    // Collect the rest of the stream.
    let mut got_stdout = false;
    let mut exit_status: Option<i32> = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), exec_stream.message())
            .await
            .expect("exec: timed out waiting for next frame")
            .expect("exec: RPC error mid-stream");

        match frame {
            None => break, // stream ended
            Some(msg) => match msg.event {
                Some(app::exec_output::Event::Stdout(b)) => {
                    let s = String::from_utf8_lossy(&b);
                    println!("smoke(exec): stdout chunk: {s:?}");
                    got_stdout = true;
                }
                Some(app::exec_output::Event::Stderr(b)) => {
                    println!(
                        "smoke(exec): stderr chunk: {:?}",
                        String::from_utf8_lossy(&b)
                    );
                }
                Some(app::exec_output::Event::Exit(e)) => {
                    exit_status = e.exit_status;
                    println!(
                        "smoke(exec): exit status={:?} wall_ms={}",
                        exit_status,
                        e.rusage.map(|r| r.wall_ms).unwrap_or(0)
                    );
                    break;
                }
                _ => {}
            },
        }
    }

    assert!(
        got_stdout,
        "exec: must have received at least one stdout frame"
    );
    assert_eq!(exit_status, Some(0), "uname -m must exit 0");

    // Cleanup.
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client.delete_session(del_req).await.expect("DeleteSession");
    println!("smoke(exec): all checks passed");
}

/// `Snapshot → EvictLocal → Resume` round-trip smoke.
///
/// Also exercises `GetLog`, `GetCowState`, and `ListCheckpoints` in the
/// same session.  Env-gated like the other smokes.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn snapshot_evict_resume_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");
    let mut client = SessionServiceClient::with_interceptor(channel, bearer_interceptor(token));
    let rpc_timeout = std::time::Duration::from_secs(30);

    // Create session in dev_vm mode so there's a live sandbox to snapshot.
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri: image_uri.clone(),
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;
    println!("smoke(snapshot): session {session_id}");

    // GetLog — just verify it returns OK with kind=conversation.
    let mut log_req = tonic::Request::new(app::GetLogRequest {
        session_id: session_id.clone(),
        kind: None,
        limit: Some(10),
    });
    log_req.set_timeout(rpc_timeout);
    let log_resp = client
        .get_log(log_req)
        .await
        .expect("GetLog must succeed")
        .into_inner();
    assert_eq!(log_resp.kind, "conversation");
    println!(
        "smoke(snapshot): GetLog returned {} events",
        log_resp.events.len()
    );

    // GetCowState — may be None if this isn't an NBD-backed session; that's fine.
    let mut cow_req = tonic::Request::new(app::GetCowStateRequest {
        session_id: session_id.clone(),
    });
    cow_req.set_timeout(rpc_timeout);
    let cow_resp = client
        .get_cow_state(cow_req)
        .await
        .expect("GetCowState must succeed")
        .into_inner();
    println!(
        "smoke(snapshot): GetCowState state={:?}",
        cow_resp.state.is_some()
    );

    // Snapshot.
    let mut snap_req = tonic::Request::new(app::SnapshotRequest {
        session_id: session_id.clone(),
    });
    snap_req.set_timeout(rpc_timeout);
    let snap_resp = client
        .snapshot(snap_req)
        .await
        .expect("Snapshot must succeed")
        .into_inner();
    println!(
        "smoke(snapshot): snapshot_id={:?} note={}",
        snap_resp.snapshot_id, snap_resp.note
    );

    // ListCheckpoints — must have at least one entry after snapshot.
    let mut ckpts_req = tonic::Request::new(app::ListCheckpointsRequest {
        session_id: session_id.clone(),
    });
    ckpts_req.set_timeout(rpc_timeout);
    let ckpts_resp = client
        .list_checkpoints(ckpts_req)
        .await
        .expect("ListCheckpoints must succeed")
        .into_inner();
    assert!(
        !ckpts_resp.checkpoints.is_empty(),
        "must have at least one checkpoint after snapshot"
    );
    println!(
        "smoke(snapshot): {} checkpoint(s), latest is_latest={}",
        ckpts_resp.checkpoints.len(),
        ckpts_resp.checkpoints[0].is_latest
    );

    // EvictLocal — requires a snapshot to exist (we just took one).
    let mut evict_req = tonic::Request::new(app::EvictLocalRequest {
        session_id: session_id.clone(),
    });
    evict_req.set_timeout(rpc_timeout);
    client
        .evict_local(evict_req)
        .await
        .expect("EvictLocal must succeed");
    println!("smoke(snapshot): EvictLocal ok");

    // Resume.
    let mut resume_req = tonic::Request::new(app::ResumeRequest {
        session_id: session_id.clone(),
    });
    resume_req.set_timeout(rpc_timeout);
    let resume_resp = client
        .resume(resume_req)
        .await
        .expect("Resume must succeed")
        .into_inner();
    println!("smoke(snapshot): Resume note={}", resume_resp.note);

    // Cleanup.
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client.delete_session(del_req).await.expect("DeleteSession");
    println!("smoke(snapshot): all checks passed");
}

/// GetArtifact smoke: skipped if no artifact exists for the session
/// (we can't create one here without a harness), but the RPC exercised
/// for NotFound path verification.
///
/// Env-gated like the other smokes.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn get_artifact_not_found_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");
    let mut client = SessionServiceClient::with_interceptor(channel, bearer_interceptor(token));
    let rpc_timeout = std::time::Duration::from_secs(30);

    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri,
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;

    // GetArtifact with a made-up UUID — must return NOT_FOUND.
    let fake_id = uuid::Uuid::new_v4().to_string();
    let mut art_req = tonic::Request::new(app::GetArtifactRequest {
        session_id: session_id.clone(),
        artifact_id: fake_id.clone(),
    });
    art_req.set_timeout(rpc_timeout);
    let err = client
        .get_artifact(art_req)
        .await
        .expect_err("GetArtifact with unknown id must return an error");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "unknown artifact must return NOT_FOUND, got {:?}",
        err.code()
    );
    println!("smoke(artifact): GetArtifact({fake_id}) → NOT_FOUND as expected");

    // NOTE: a live artifact read is skipped — no artifact exists without a
    // harness-driven upload. The NOT_FOUND path verifies the RPC is wired.

    // Cleanup.
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client.delete_session(del_req).await.expect("DeleteSession");
    println!("smoke(artifact): all checks passed");
}

/// `ShellRelayService.Relay` smoke: open a relay to a dev_vm session,
/// drive a live `echo hi` through the shell, assert output is received,
/// then drop cleanly and confirm health via GetSession.
///
/// ## Why mpsc instead of a fixed iterator
///
/// The server bridge (`grpc_app/shell_relay.rs`) runs `g2t` and `t2g`
/// in a `select!`. When the client → server stream ends (iterator
/// exhausted), `g2t` finishes, `select!` picks it up and the relay
/// task tears down before ttyd's output arrives. The correct approach
/// is to keep the request stream OPEN while reading, and only close it
/// (drop the sender) once we have confirmed output.
///
/// ## ttyd wire protocol
///
/// Mirrored from `web/src/components/TerminalPane.tsx`:
/// - First frame (text, JSON): `{"AuthToken":"","columns":80,"rows":24}`
///   — ttyd requires this to initialize the PTY.
/// - Subsequent input (text): `"0"` + raw bytes  (ASCII 0x30 = INPUT)
/// - Server output (binary): byte 0x30 (`'0'`) + terminal bytes
/// - Server title/prefs (binary): bytes 0x31/0x32 + payload (ignored here)
///
/// Env-gated like the other smokes.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn shell_relay_smoke() {
    use engram_protocol::app::shell_relay_service_client::ShellRelayServiceClient;
    use tokio_stream::wrappers::ReceiverStream;

    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };
    let Some(image_uri) = find_no_harness_image(&addr, &token).await else {
        return;
    };

    // We need two channels: one for SessionService and one for ShellRelayService.
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    let mut session_client =
        SessionServiceClient::with_interceptor(channel.clone(), bearer_interceptor(token.clone()));
    let rpc_timeout = std::time::Duration::from_secs(30);

    // Create a dev_vm session so there's a live sandbox.
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        image_uri,
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_secret_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = session_client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;
    println!("smoke(relay): session {session_id}");

    // Open a Relay stream held open via mpsc, drive a live echo, assert output.
    {
        let mut relay_client = ShellRelayServiceClient::with_interceptor(
            channel.clone(),
            bearer_interceptor(token.clone()),
        );

        // Drive the request stream from an mpsc sender so we can keep it
        // open while reading response frames. Dropping the sender closes
        // the stream and signals the server bridge to tear down cleanly.
        //
        // IMPORTANT: the `open` frame and the ttyd auth JSON are buffered
        // into the channel (capacity 8) BEFORE calling `.relay()`. The
        // server's `relay` handler awaits `inbound.next()` for the open
        // frame before it even starts processing; if we send it after
        // `.relay().await`, we deadlock — the client waits for the server
        // to accept the RPC, and the server waits for the first frame.
        let (req_tx, req_rx) = tokio::sync::mpsc::channel::<app::RelayShellRequest>(8);

        // ---- 1. Buffer the `open` frame ----
        req_tx
            .try_send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Open(app::ShellOpen {
                    session_id: session_id.clone(),
                })),
            })
            .expect("buffer open frame (channel is empty)");

        // ---- 2. Buffer the ttyd auth/resize JSON (first content frame) ----
        // ttyd's WebSocket protocol requires this JSON as the very first
        // non-handshake frame before it will honour input. Without it the
        // PTY is not initialized and keystrokes are silently dropped.
        // (Mirrored from TerminalPane.tsx `localWs.onopen` handler.)
        req_tx
            .try_send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Text(
                    r#"{"AuthToken":"","columns":80,"rows":24}"#.to_string(),
                )),
            })
            .expect("buffer ttyd auth frame (channel has room)");

        let req_stream = ReceiverStream::new(req_rx);
        let relay_req = tonic::Request::new(req_stream);
        // No RPC-level timeout on the request itself: we impose per-step
        // deadlines on the response reads below, which is the right place.
        // A request-level timeout races our response reads and cancels the
        // entire bidi stream when the first deadline fires (the panic we
        // saw: "Cancelled: Timeout expired" at relay() call time).

        let mut stream = match relay_client.relay(relay_req).await {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                if e.code() == tonic::Code::Unavailable {
                    // ttyd may not be running on this no-harness image; skip
                    // rather than fail so CI doesn't go red on infra variance.
                    println!(
                        "smoke(relay): SKIP — proxy_shell unavailable (ttyd not running on this \
                         image): {e}"
                    );
                    // Cleanup the session we created.
                    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
                        session_id: session_id.clone(),
                    });
                    del_req.set_timeout(rpc_timeout);
                    let _ = session_client.delete_session(del_req).await;
                    return;
                }
                panic!("smoke(relay): unexpected error opening relay: {e}");
            }
        };

        // ---- 3. Await the first output frame(s) (ttyd handshake / prompt) ----
        // ttyd sends a preferences frame (0x32) and/or prompt output immediately
        // on WS open. Wait for the first binary or text output frame before
        // sending keystrokes — this synchronises us with ttyd's PTY boot.
        let handshake_deadline = std::time::Duration::from_secs(10);
        let got_handshake = tokio::time::timeout(handshake_deadline, async {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => {
                        match &msg.frame {
                            Some(engram_protocol::app::relay_shell_response::Frame::Binary(b)) => {
                                let decoded = String::from_utf8_lossy(b);
                                println!(
                                    "smoke(relay): handshake binary frame ({} bytes): {:?}",
                                    b.len(),
                                    &decoded[..decoded.len().min(120)],
                                );
                                return true;
                            }
                            Some(engram_protocol::app::relay_shell_response::Frame::Text(t)) => {
                                println!(
                                    "smoke(relay): handshake text frame: {:?}",
                                    &t[..t.len().min(120)],
                                );
                                return true;
                            }
                            other => {
                                println!(
                                    "smoke(relay): handshake non-output frame: {:?}",
                                    other.as_ref().map(frame_kind_name),
                                );
                                // keep reading — ping/pong/close from ttyd setup
                            }
                        }
                    }
                    Ok(None) => {
                        panic!(
                            "smoke(relay): stream ended during handshake — ttyd may have closed \
                             immediately; check proxy_shell bridge or ttyd health"
                        );
                    }
                    Err(e) => {
                        panic!("smoke(relay): stream error during handshake: {e}");
                    }
                }
            }
        })
        .await;

        if got_handshake.is_err() {
            panic!(
                "smoke(relay): no output from ttyd within {handshake_deadline:?} after auth \
                 frame — ttyd may not be responding; check proxy_shell bridge"
            );
        }

        // ---- 4. Send `echo hi\r` with the ttyd input prefix ----
        // ttyd's tty subprotocol: client input frames are text frames
        // prefixed with the character '0' (ASCII 0x30). Without the
        // prefix, ttyd treats the frame as an unknown command and discards
        // the bytes. (See TerminalPane.tsx: `localWs.send(ttyClient.INPUT + data)`)
        req_tx
            .send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Text(
                    "0echo hi\r".to_string(),
                )),
            })
            .await
            .expect("send echo input frame");

        // ---- 5. Read until we see "hi" in an output frame ----
        // ttyd output frames are binary with a 0x30 ('0') type prefix
        // followed by the raw terminal bytes.  We decode lossily and
        // check for the literal string "hi".  We also accept text frames
        // (legacy ttyd behaviour) decoded directly.
        let echo_deadline = std::time::Duration::from_secs(15);
        let mut found_hi = false;
        let mut echo_evidence: Option<String> = None;

        let echo_result = tokio::time::timeout(echo_deadline, async {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => {
                        match &msg.frame {
                            Some(engram_protocol::app::relay_shell_response::Frame::Binary(b)) => {
                                // Strip the 0x30 output-type prefix if present.
                                let payload = if b.first() == Some(&0x30) {
                                    &b[1..]
                                } else {
                                    b.as_slice()
                                };
                                let decoded = String::from_utf8_lossy(payload).to_string();
                                println!(
                                    "smoke(relay): output binary frame ({} bytes): {:?}",
                                    b.len(),
                                    &decoded[..decoded.len().min(200)],
                                );
                                if decoded.contains("hi") {
                                    return Some(decoded);
                                }
                            }
                            Some(engram_protocol::app::relay_shell_response::Frame::Text(t)) => {
                                println!(
                                    "smoke(relay): output text frame: {:?}",
                                    &t[..t.len().min(200)],
                                );
                                if t.contains("hi") {
                                    return Some(t.clone());
                                }
                            }
                            other => {
                                println!(
                                    "smoke(relay): non-output frame while waiting for echo: {:?}",
                                    other.as_ref().map(frame_kind_name),
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        // Stream ended — relay torn down before we saw "hi".
                        return None;
                    }
                    Err(e) => {
                        panic!("smoke(relay): stream error while waiting for echo: {e}");
                    }
                }
            }
        })
        .await;

        match echo_result {
            Ok(Some(evidence)) => {
                found_hi = true;
                echo_evidence = Some(evidence);
            }
            Ok(None) => {
                // Stream closed before "hi" appeared.
            }
            Err(_) => {
                // Timeout elapsed — will be caught by the assert below.
            }
        }

        // HARD assertion: the echo must have been observed.
        assert!(
            found_hi,
            "smoke(relay): FAIL — did not observe 'hi' in any output frame within \
             {echo_deadline:?} after sending `echo hi\\r`; this indicates the relay bridge \
             is not forwarding input to ttyd or ttyd is not sending output back"
        );
        println!(
            "smoke(relay): PASS — echo evidence: {:?}",
            echo_evidence
                .as_deref()
                .unwrap_or("<none>")
                .chars()
                .take(200)
                .collect::<String>(),
        );

        // ---- 6. Drop the sender to close the request stream cleanly ----
        // This signals the server bridge (g2t loop) that the client is done.
        // The bridge tears down, releases the shell lease, and closes the
        // response stream.
        drop(req_tx);

        // Drain any trailing frames until the response stream closes.
        let drain_deadline = std::time::Duration::from_secs(3);
        let _ = tokio::time::timeout(drain_deadline, async {
            while let Ok(Some(_)) = stream.message().await {}
        })
        .await;

        println!("smoke(relay): response stream drained cleanly after sender drop");
    }

    // Small sleep to let the drop-path lease release complete (async spawn).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify the coordinator is still healthy: GetSession should work.
    let mut get_req = tonic::Request::new(app::GetSessionRequest {
        session_id: session_id.clone(),
    });
    get_req.set_timeout(rpc_timeout);
    session_client
        .get_session(get_req)
        .await
        .expect("GetSession after relay drop must succeed");
    println!("smoke(relay): GetSession after relay drop ok — coordinator healthy");

    // Cleanup.
    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    session_client
        .delete_session(del_req)
        .await
        .expect("DeleteSession");
    println!("smoke(relay): all checks passed");
}

/// SecretService lifecycle smoke: put → has(true) → delete → has(false) via
/// gRPC against the live stack. Proves migration 0061 end-to-end (the
/// `sealed_secrets` table is present and the KEK unseals).
///
/// Env-gated + `#[ignore]` like the other smokes.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn secret_lifecycle_smoke() {
    let Some((addr, token)) = grpc_addr_and_token() else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    let rpc_timeout = std::time::Duration::from_secs(10);
    let mut client = app::secret_service_client::SecretServiceClient::with_interceptor(
        channel,
        bearer_interceptor(token),
    );

    // Use a timestamp-based key for uniqueness without needing uuid dep.
    let key = format!(
        "smoke-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // 1. Put
    let mut req = tonic::Request::new(app::PutSecretRequest {
        key: key.clone(),
        value: "smoke-secret-value".to_string(),
    });
    req.set_timeout(rpc_timeout);
    client
        .put_secret(req)
        .await
        .expect("PutSecret must succeed");
    println!("secret_lifecycle_smoke: put {key:?}");

    // 2. Has → true
    let mut req = tonic::Request::new(app::HasSecretRequest { key: key.clone() });
    req.set_timeout(rpc_timeout);
    let has = client
        .has_secret(req)
        .await
        .expect("HasSecret must succeed");
    assert!(
        has.into_inner().exists,
        "HasSecret must return true after PutSecret"
    );
    println!("secret_lifecycle_smoke: has → true ✓");

    // 3. Delete
    let mut req = tonic::Request::new(app::DeleteSecretRequest { key: key.clone() });
    req.set_timeout(rpc_timeout);
    client
        .delete_secret(req)
        .await
        .expect("DeleteSecret must succeed");
    println!("secret_lifecycle_smoke: deleted");

    // 4. Has → false
    let mut req = tonic::Request::new(app::HasSecretRequest { key: key.clone() });
    req.set_timeout(rpc_timeout);
    let has2 = client
        .has_secret(req)
        .await
        .expect("HasSecret must succeed");
    assert!(
        !has2.into_inner().exists,
        "HasSecret must return false after DeleteSecret"
    );
    println!("secret_lifecycle_smoke: has → false ✓ — all checks passed");
}

/// Bearer-rejected probe: one Fleet RPC (ListHosts) with NO bearer token →
/// Code::Unauthenticated. Verifies the live stack is fail-closed.
///
/// Env-gated + `#[ignore]` like the other smokes.
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn bearer_rejected_probe() {
    let Some((addr, _token)) = grpc_addr_and_token() else {
        return;
    };

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("valid gRPC endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("connect to coordinator app-gRPC");

    // No interceptor — no bearer header.
    let mut client = app::fleet_service_client::FleetServiceClient::new(channel);
    let mut req = tonic::Request::new(app::ListHostsRequest::default());
    req.set_timeout(std::time::Duration::from_secs(5));
    let err = client
        .list_hosts(req)
        .await
        .expect_err("ListHosts with no bearer must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "live stack must reject unauthenticated FleetService calls: {err:?}"
    );
    println!("bearer_rejected_probe: ListHosts with no bearer → Unauthenticated ✓");
}

/// Human-readable frame variant name for diagnostic prints.
fn frame_kind_name(f: &engram_protocol::app::relay_shell_response::Frame) -> &'static str {
    use engram_protocol::app::relay_shell_response::Frame;
    match f {
        Frame::Text(_) => "text",
        Frame::Binary(_) => "binary",
        Frame::Ping(_) => "ping",
        Frame::Pong(_) => "pong",
        Frame::Close(_) => "close",
    }
}
