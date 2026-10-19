//! The client against an in-process stub of the orchestrator: unary calls
//! with and without an error, a server stream (happy path and an error in
//! the end-of-stream envelope), the SSE tail, and the device flow. Loopback
//! only; no network.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use prost::Message;

use engram_app_client::connect::{end_stream_err, end_stream_ok, envelope};
use engram_app_client::device_flow::{poll_token, request_grant, PollError};
use engram_app_client::proto::*;
use engram_app_client::sse::EventStream;
use engram_app_client::{Auth, Clients, Code, Transport};

#[derive(Default)]
struct Seen {
    token_polls: AtomicU32,
}

async fn who_am_i(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let req = WhoAmIRequest::decode(body).expect("request decodes");
    let _ = req;
    assert_eq!(headers.get("content-type").unwrap(), "application/proto");
    assert_eq!(headers.get("connect-protocol-version").unwrap(), "1");
    // A WhoAmI encodes to zero bytes; the length must still be on the wire
    // (a Google front end answers a length-less HTTP/1.1 POST with 411).
    assert_eq!(headers.get("content-length").unwrap(), "0");
    let key = headers
        .get("x-api-key")
        .map(|v| v.to_str().unwrap().to_string());
    let resp = WhoAmIResponse {
        user_id: "u-1".into(),
        email: format!("key={}", key.unwrap_or_default()),
        name: "A".into(),
        role: "user".into(),
        service_account: false,
    };
    (
        [(header::CONTENT_TYPE, "application/proto")],
        resp.encode_to_vec(),
    )
}

async fn get_session_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"code":"not_found","message":"no such session"}"#,
    )
}

async fn exec(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/connect+proto"
    );
    // One request envelope: flags 0, len, payload.
    assert_eq!(body[0], 0);
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    let req = ExecRequest::decode(&body[5..5 + len]).expect("request decodes");
    let mut out = Vec::new();
    let frame =
        |ev: exec_output::Event| envelope(0, &ExecOutput { event: Some(ev) }.encode_to_vec());
    out.extend(frame(exec_output::Event::Started(ExecStarted {
        exec_id: "e-1".into(),
    })));
    out.extend(frame(exec_output::Event::Stdout(b"out-a\n".to_vec())));
    out.extend(frame(exec_output::Event::Stderr(b"err-b\n".to_vec())));
    if req.command.as_deref() == Some("fail-mid-stream") {
        out.extend(end_stream_err("aborted", "sandbox went away"));
    } else {
        out.extend(frame(exec_output::Event::Exit(ExecExit {
            exit_status: Some(3),
            rusage: None,
        })));
        out.extend(end_stream_ok());
    }
    ([(header::CONTENT_TYPE, "application/connect+proto")], out)
}

async fn events(headers: HeaderMap) -> impl IntoResponse {
    assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
    assert_eq!(headers.get("x-api-key").unwrap(), "engk_test");
    let body = concat!(
        "event: ping\ndata: \n\n",
        "id: 0\nevent: run_started\ndata: {\"idx\":0,\"kind\":\"run_started\",\"payload_json\":\"{}\"}\n\n",
        "id: 1\nevent: agent_message\ndata: {\"idx\":1}\n\n",
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], body)
}

async fn device_code() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"device_code":"dev-code","user_code":"ABCD1234","verification_uri":"http://127.0.0.1:1/device","verification_uri_complete":"http://127.0.0.1:1/device?user_code=ABCD1234","expires_in":600,"interval":0}"#,
    )
}

async fn device_token(State(seen): State<Arc<Seen>>, body: Bytes) -> impl IntoResponse {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(v["client_id"], "engrams-cli");
    assert_eq!(v["device_code"], "dev-code");
    let n = seen.token_polls.fetch_add(1, Ordering::SeqCst) + 1;
    if n < 2 {
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"authorization_pending"}"#.to_string(),
        )
    } else {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"access_token":"session-token-xyz","token_type":"Bearer"}"#.to_string(),
        )
    }
}

async fn start_stub() -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/rpc/engram.app.v1.ApiKeyService/WhoAmI", post(who_am_i))
        .route(
            "/rpc/engram.app.v1.SessionService/GetSession",
            post(get_session_not_found),
        )
        .route("/rpc/engram.app.v1.SessionService/Exec", post(exec))
        .route("/api/v1/sessions/{id}/events", get(events))
        .route("/api/auth/device/code", post(device_code))
        .route("/api/auth/device/token", post(device_token))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn unary_round_trip_sends_the_api_key() {
    let (host, _) = start_stub().await;
    let c = Clients::new(Transport::new(host, Auth::ApiKey("engk_test".into())));
    let me = c.who_am_i(WhoAmIRequest {}).await.unwrap();
    assert_eq!(me.email, "key=engk_test");
    assert_eq!(me.user_id, "u-1");
}

#[tokio::test]
async fn unary_error_body_maps_to_code_and_message() {
    let (host, _) = start_stub().await;
    let c = Clients::new(Transport::new(host, Auth::None));
    let err = c
        .get_session(GetSessionRequest {
            session_id: "x".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, Code::NotFound);
    assert_eq!(err.to_string(), "NotFound: no such session");
}

#[tokio::test]
async fn unimplemented_route_maps_404_to_unimplemented() {
    let (host, _) = start_stub().await;
    let c = Clients::new(Transport::new(host, Auth::None));
    let err = c.list_hosts(ListHostsRequest {}).await.unwrap_err();
    assert_eq!(err.code, Code::Unimplemented);
    assert_eq!(err.to_string(), "Unimplemented: HTTP 404");
}

#[tokio::test]
async fn connection_refused_is_unavailable() {
    let c = Clients::new(Transport::new("http://127.0.0.1:1", Auth::None));
    let err = c.list_hosts(ListHostsRequest {}).await.unwrap_err();
    assert_eq!(err.code, Code::Unavailable);
}

#[tokio::test]
async fn server_stream_yields_every_envelope_then_ends() {
    let (host, _) = start_stub().await;
    let c = Clients::new(Transport::new(host, Auth::None));
    let mut s = c
        .exec(ExecRequest {
            session_id: "s".into(),
            command: Some("ls".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while let Some(item) = s.next().await {
        let msg = item.unwrap();
        kinds.push(match msg.event.unwrap() {
            exec_output::Event::Started(_) => "started".to_string(),
            exec_output::Event::Stdout(b) => format!("stdout:{}", String::from_utf8(b).unwrap()),
            exec_output::Event::Stderr(b) => format!("stderr:{}", String::from_utf8(b).unwrap()),
            exec_output::Event::Exit(e) => format!("exit:{:?}", e.exit_status),
        });
    }
    assert_eq!(
        kinds,
        [
            "started",
            "stdout:out-a\n",
            "stderr:err-b\n",
            "exit:Some(3)"
        ]
    );
    assert!(s.next().await.is_none(), "a finished stream stays finished");
}

#[tokio::test]
async fn server_stream_end_envelope_error_is_the_last_item() {
    let (host, _) = start_stub().await;
    let c = Clients::new(Transport::new(host, Auth::None));
    let mut s = c
        .exec(ExecRequest {
            session_id: "s".into(),
            command: Some("fail-mid-stream".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut n = 0;
    let mut last_err = None;
    while let Some(item) = s.next().await {
        match item {
            Ok(_) => n += 1,
            Err(e) => last_err = Some(e),
        }
    }
    assert_eq!(n, 3);
    let e = last_err.expect("end-of-stream error surfaces");
    assert_eq!(e.code, Code::Aborted);
    assert_eq!(e.message, "sandbox went away");
}

#[tokio::test]
async fn sse_tail_parses_frames_in_order() {
    let (host, _) = start_stub().await;
    let t = Transport::new(host, Auth::ApiKey("engk_test".into()));
    let mut s = EventStream::open(&t, "sess-1", Some(5), None)
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Some(f) = s.next().await.unwrap() {
        got.push((f.id, f.event.unwrap(), f.data.join("\n")));
    }
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (None, "ping".to_string(), String::new()));
    assert_eq!(got[1].0.as_deref(), Some("0"));
    assert_eq!(got[1].1, "run_started");
    assert!(got[1].2.contains("\"payload_json\""));
    assert_eq!(got[2].0.as_deref(), Some("1"));
}

#[tokio::test]
async fn device_flow_polls_through_pending_to_a_token() {
    let (host, seen) = start_stub().await;
    let http = engram_tls::client();
    let grant = request_grant(&http, &host).await.unwrap();
    assert_eq!(grant.user_code, "ABCD1234");
    assert_eq!(grant.url(), "http://127.0.0.1:1/device?user_code=ABCD1234");
    let token = poll_token(&http, &host, &grant).await.unwrap();
    assert_eq!(token, "session-token-xyz");
    assert_eq!(seen.token_polls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn device_flow_expired_grant_fails_fast() {
    let (host, _) = start_stub().await;
    let http = engram_tls::client();
    let mut grant = request_grant(&http, &host).await.unwrap();
    grant.expires_in = 0;
    match poll_token(&http, &host, &grant).await {
        Err(PollError::Expired) => {}
        other => panic!("expected Expired, got {other:?}"),
    }
}
