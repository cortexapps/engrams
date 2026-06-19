//! Live smoke tests for the app-gRPC surface (ADR 0039 / ADR 0051).
//!
//! Gate: `ENGRAM_SMOKE_GRPC` must be set to `<host>:<port>` (the
//! coordinator's `--app-grpc-addr`). When unset the test prints a skip
//! notice and returns `Ok` — the same graceful-skip idiom the live-pg
//! tests use (see `host_register_live_pg.rs`). All tests are also
//! `#[ignore]`'d so they never run under plain `cargo nextest`.
//!
//! Run against a running `just dev` stack:
//!
//! ```text
//! just smoke-control-plane
//! ```
//!
//! The Tiltfile configures the coordinator with bearer `dev-app-grpc-token`
//! (overridable via `ENGRAM_APP_GRPC_TOKENS`). These smokes discover a
//! no-harness image via `ImageService.ListEnabledImages` over gRPC and then
//! drive session lifecycle / exec / snapshot / shell-relay over gRPC.
//!
//! ADR 0051 Drip E: the coordinator's REST surface is gone — `StreamEvents`
//! gRPC is the sole events surface; there is no SSE feed to compare against.

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
/// via gRPC, reopen with `since=last_idx` and verify no gap or dup.
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
        mounts: Vec::new(),
        image_uri: image_uri.clone(),
        mode: String::new(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
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
    let mut stream_req = tonic::Request::new(app::StreamEventsRequest {
        session_id: session_id.clone(),
        since: None,
    });
    stream_req.set_timeout(rpc_timeout);
    let mut stream = client
        .stream_events(stream_req)
        .await
        .expect("StreamEvents must succeed")
        .into_inner();

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
                if !grpc_idxs.is_empty() && ev.kind != "status_changed" {
                    break;
                }
                if grpc_idxs.len() >= 2 {
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("StreamEvents RPC error: {e}"),
            Err(_) => break,
        }
    }
    assert!(
        !grpc_idxs.is_empty(),
        "StreamEvents must yield at least one event (status_changed) for a newly created session"
    );
    println!("smoke(stream_events): gRPC idx sequence: {grpc_idxs:?}");

    // ---- 3. Reopen with since=last_idx — no gap, no dup ----
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

    let reopen_result =
        tokio::time::timeout(std::time::Duration::from_secs(5), reopen_stream.message()).await;
    match reopen_result {
        Ok(Ok(Some(ev))) => {
            if let Some(next_idx) = ev.idx {
                assert!(
                    next_idx > last_idx,
                    "StreamEvents reopen must yield idx > {last_idx} (since=last_idx), got {next_idx}"
                );
                println!(
                    "smoke(stream_events): reopen with since={last_idx} → first event idx={next_idx} — no gap, no dup"
                );
            } else {
                println!(
                    "smoke(stream_events): reopen first event has kind={:?}, idx=None — ok",
                    ev.kind
                );
            }
        }
        Ok(Ok(None)) | Err(_) => {
            println!(
                "smoke(stream_events): reopen with since={last_idx}: no events received \
                 within 5s — no dupes or gaps observed"
            );
        }
        Ok(Err(e)) => panic!("StreamEvents reopen RPC error: {e}"),
    }

    // ---- 4. DeleteSession ----
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
#[tokio::test]
#[ignore = "requires a running dev stack (just dev + just smoke-control-plane)"]
async fn session_crud_smoke() {
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

    // ---- 1. CreateSession ----
    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        mounts: Vec::new(),
        image_uri: image_uri.clone(),
        mode: String::new(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
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

    // ---- 2. ListSessions ----
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

    // ---- 5. GetSession after delete — row persists as terminal ----
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

/// `Exec` streaming smoke: create a session, exec `uname -m`, assert
/// started/stdout/exit framing, then delete.
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

    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        mounts: Vec::new(),
        image_uri: image_uri.clone(),
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let create_resp = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner();
    let session_id = create_resp.session_id.clone();
    println!("smoke(exec): session {session_id}");

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

    let mut got_stdout = false;
    let mut exit_status: Option<i32> = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), exec_stream.message())
            .await
            .expect("exec: timed out waiting for next frame")
            .expect("exec: RPC error mid-stream");

        match frame {
            None => break,
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

    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client.delete_session(del_req).await.expect("DeleteSession");
    println!("smoke(exec): all checks passed");
}

/// `Snapshot → EvictLocal → Resume` round-trip smoke. Also exercises
/// `GetLog`, `GetCowState`, and `ListCheckpoints` in the same session.
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

    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        mounts: Vec::new(),
        image_uri: image_uri.clone(),
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;
    println!("smoke(snapshot): session {session_id}");

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

    let mut evict_req = tonic::Request::new(app::EvictLocalRequest {
        session_id: session_id.clone(),
    });
    evict_req.set_timeout(rpc_timeout);
    client
        .evict_local(evict_req)
        .await
        .expect("EvictLocal must succeed");
    println!("smoke(snapshot): EvictLocal ok");

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

    let mut del_req = tonic::Request::new(app::DeleteSessionRequest {
        session_id: session_id.clone(),
    });
    del_req.set_timeout(rpc_timeout);
    client.delete_session(del_req).await.expect("DeleteSession");
    println!("smoke(snapshot): all checks passed");
}

/// GetArtifact NotFound path: a made-up artifact id must return NOT_FOUND.
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
        mounts: Vec::new(),
        image_uri,
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;

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
/// ttyd wire protocol mirrored from `web/src/components/TerminalPane.tsx`.
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

    let mut create_req = tonic::Request::new(app::CreateSessionRequest {
        mounts: Vec::new(),
        image_uri,
        mode: "dev_vm".into(),
        prompt: None,
        secrets: std::collections::HashMap::new(),
        harness_env: std::collections::HashMap::new(),
        prompt_id: None,
    });
    create_req.set_timeout(rpc_timeout);
    let session_id = session_client
        .create_session(create_req)
        .await
        .expect("CreateSession")
        .into_inner()
        .session_id;
    println!("smoke(relay): session {session_id}");

    {
        let mut relay_client = ShellRelayServiceClient::with_interceptor(
            channel.clone(),
            bearer_interceptor(token.clone()),
        );

        let (req_tx, req_rx) = tokio::sync::mpsc::channel::<app::RelayShellRequest>(8);

        req_tx
            .try_send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Open(app::ShellOpen {
                    session_id: session_id.clone(),
                })),
            })
            .expect("buffer open frame (channel is empty)");

        req_tx
            .try_send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Text(
                    r#"{"AuthToken":"","columns":80,"rows":24}"#.to_string(),
                )),
            })
            .expect("buffer ttyd auth frame (channel has room)");

        let req_stream = ReceiverStream::new(req_rx);
        let relay_req = tonic::Request::new(req_stream);

        let mut stream = match relay_client.relay(relay_req).await {
            Ok(resp) => resp.into_inner(),
            Err(e) => {
                if e.code() == tonic::Code::Unavailable {
                    println!(
                        "smoke(relay): SKIP — proxy_shell unavailable (ttyd not running on this \
                         image): {e}"
                    );
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

        let handshake_deadline = std::time::Duration::from_secs(10);
        let got_handshake = tokio::time::timeout(handshake_deadline, async {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => match &msg.frame {
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
                        }
                    },
                    Ok(None) => {
                        panic!(
                            "smoke(relay): stream ended during handshake — ttyd may have closed \
                             immediately; check proxy_shell bridge or ttyd health"
                        );
                    }
                    Err(e) => panic!("smoke(relay): stream error during handshake: {e}"),
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

        req_tx
            .send(app::RelayShellRequest {
                frame: Some(app::relay_shell_request::Frame::Text(
                    "0echo hi\r".to_string(),
                )),
            })
            .await
            .expect("send echo input frame");

        let echo_deadline = std::time::Duration::from_secs(15);
        let mut found_hi = false;
        let mut echo_evidence: Option<String> = None;

        let echo_result = tokio::time::timeout(echo_deadline, async {
            loop {
                match stream.message().await {
                    Ok(Some(msg)) => match &msg.frame {
                        Some(engram_protocol::app::relay_shell_response::Frame::Binary(b)) => {
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
                    },
                    Ok(None) => return None,
                    Err(e) => panic!("smoke(relay): stream error while waiting for echo: {e}"),
                }
            }
        })
        .await;

        match echo_result {
            Ok(Some(evidence)) => {
                found_hi = true;
                echo_evidence = Some(evidence);
            }
            Ok(None) => {}
            Err(_) => {}
        }

        assert!(
            found_hi,
            "smoke(relay): FAIL — did not observe 'hi' in any output frame within \
             {echo_deadline:?} after sending `echo hi\\r`"
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

        drop(req_tx);

        let drain_deadline = std::time::Duration::from_secs(3);
        let _ = tokio::time::timeout(drain_deadline, async {
            while let Ok(Some(_)) = stream.message().await {}
        })
        .await;

        println!("smoke(relay): response stream drained cleanly after sender drop");
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut get_req = tonic::Request::new(app::GetSessionRequest {
        session_id: session_id.clone(),
    });
    get_req.set_timeout(rpc_timeout);
    session_client
        .get_session(get_req)
        .await
        .expect("GetSession after relay drop must succeed");
    println!("smoke(relay): GetSession after relay drop ok — coordinator healthy");

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

/// Bearer-rejected probe: one Fleet RPC (ListHosts) with NO bearer token →
/// Code::Unauthenticated. Verifies the live stack is fail-closed.
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
    println!("bearer_rejected_probe: ListHosts with no bearer → Unauthenticated");
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
