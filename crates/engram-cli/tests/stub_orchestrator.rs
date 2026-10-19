//! The real binary against a STUB orchestrator: an axum server serving
//! Connect handlers on /rpc plus hand-rolled /api/auth device-flow + SSE
//! routes. Each test spawns the `engrams` binary with ENGRAMS_URL pointed at
//! the stub and asserts the stdout/exit-code contract scripts rely on.
//! Loopback only; no network.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use prost::Message;

use engram_app_client::connect::{end_stream_ok, envelope};
use engram_app_client::proto::*;

/// One request the stub saw, for header assertions.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    api_key: Option<String>,
    authorization: Option<String>,
}

#[derive(Default)]
struct Stub {
    seen: Mutex<Vec<Seen>>,
    token_polls: AtomicU32,
}

fn proto<M: Message>(m: M) -> Response {
    (
        [(header::CONTENT_TYPE, "application/proto")],
        m.encode_to_vec(),
    )
        .into_response()
}

fn json(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn hdr(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| v.to_str().unwrap().to_string())
}

async fn dispatch(
    State(stub): State<Arc<Stub>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    stub.seen.lock().unwrap().push(Seen {
        path: path.clone(),
        api_key: hdr(&headers, "x-api-key"),
        authorization: hdr(&headers, "authorization"),
    });
    match path.as_str() {
        "/rpc/engram.app.v1.SessionService/ListSessions" => proto(ListSessionsResponse {
            sessions: vec![SessionListItem {
                session: Some(Session {
                    id: "sess-1".into(),
                    status: "active".into(),
                    image: "ghcr.io/x/y:z".into(),
                    mode: "agent".into(),
                    created_at: "2026-07-10T00:00:00Z".into(),
                    last_active_at: "2026-07-10T00:01:00Z".into(),
                    sandbox_id: Some("sb-1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }),
        "/rpc/engram.app.v1.SessionService/DeleteSession" => {
            proto(DeleteSessionResponse::default())
        }
        // Streams two stdout chunks, one stderr, exit 3.
        "/rpc/engram.app.v1.SessionService/Exec" => {
            let frame = |ev: exec_output::Event| {
                envelope(0, &ExecOutput { event: Some(ev) }.encode_to_vec())
            };
            let mut out = Vec::new();
            out.extend(frame(exec_output::Event::Started(ExecStarted {
                exec_id: "e-1".into(),
            })));
            out.extend(frame(exec_output::Event::Stdout(b"out-a\n".to_vec())));
            out.extend(frame(exec_output::Event::Stderr(b"err-b\n".to_vec())));
            out.extend(frame(exec_output::Event::Stdout(b"out-c\n".to_vec())));
            out.extend(frame(exec_output::Event::Exit(ExecExit {
                exit_status: Some(3),
                rusage: None,
            })));
            out.extend(end_stream_ok());
            ([(header::CONTENT_TYPE, "application/connect+proto")], out).into_response()
        }
        // uint64 capacities: the field class that crashed `--json hosts list`
        // in the Bun CLI's e2e gate. This stub pins the output edge.
        "/rpc/engram.app.v1.FleetService/ListHosts" => proto(ListHostsResponse {
            hosts: vec![HostView {
                id: "host-1".into(),
                hostname: "stub-host".into(),
                status: "ready".into(),
                capacity_total_mib: 16384,
                capacity_used_mib: 512,
                allocatable_mib: 16384,
                reserved_mib: 512,
                free_mib: 15872,
                util_base_shm_mib: 1024,
                cpu_budget_vcpus: 64,
                free_vcpus: 56,
                cordoned: false,
                running_sandboxes: 2,
                last_heartbeat_at: "2026-07-10T00:00:00Z".into(),
                ..Default::default()
            }],
        }),
        "/rpc/engram.app.v1.ProfileService/ListProfiles" => proto(ListProfilesResponse {
            profiles: ["prof-1:backend", "prof-2:dupe", "prof-3:dupe"]
                .iter()
                .map(|s| {
                    let (id, name) = s.split_once(':').unwrap();
                    Profile {
                        id: id.into(),
                        name: name.into(),
                        archived: false,
                        ..Default::default()
                    }
                })
                .collect(),
        }),
        "/rpc/engram.app.v1.TaskService/CreateTask" => {
            let req = CreateTaskRequest::decode(body).unwrap();
            proto(CreateTaskResponse {
                task: Some(Task {
                    id: "task-1".into(),
                    r#type: "chat".into(),
                    status: "open".into(),
                    created_at: "2026-07-10T00:00:00Z".into(),
                    source_json: "{}".into(),
                    sessions: vec![TaskSessionRef {
                        session_id: format!("sess-for-{}", req.profile_id),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            })
        }
        "/rpc/engram.app.v1.ApiKeyService/CreateCliKey" => proto(CreateCliKeyResponse {
            meta: Some(ApiKeyMeta {
                id: "key-1".into(),
                name: "cli:test".into(),
                role: "user".into(),
                start: "engk_abc123".into(),
                created_at: "2026-07-10T00:00:00Z".into(),
                ..Default::default()
            }),
            key: "engk_minted-plaintext".into(),
        }),
        "/rpc/engram.app.v1.ApiKeyService/RevokeCliKey" => {
            proto(RevokeCliKeyResponse { revoked: true })
        }
        "/rpc/engram.app.v1.ApiKeyService/WhoAmI" => proto(WhoAmIResponse {
            user_id: "u-alice".into(),
            email: "alice@example.com".into(),
            name: "Alice".into(),
            role: "user".into(),
            service_account: false,
        }),
        "/api/auth/device/code" => json(
            StatusCode::OK,
            r#"{"device_code":"dev-code","user_code":"ABCD1234","verification_uri":"http://127.0.0.1:1/device","verification_uri_complete":"http://127.0.0.1:1/device?user_code=ABCD1234","expires_in":600,"interval":0}"#,
        ),
        // First poll pending, then approved — exercises the poll loop.
        "/api/auth/device/token" => {
            if stub.token_polls.fetch_add(1, Ordering::SeqCst) + 1 < 2 {
                json(
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"authorization_pending"}"#,
                )
            } else {
                json(
                    StatusCode::OK,
                    r#"{"access_token":"session-token-xyz","token_type":"Bearer"}"#,
                )
            }
        }
        // The frames the orchestrator's events route writes: `data:` is the
        // `{idx, kind, payload_json}` envelope, and a lagged frame has no
        // `id:` line and a null idx.
        p if p.starts_with("/api/v1/sessions/") && p.ends_with("/events") => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: ping\ndata: \n\n",
                "id: 0\nevent: run_started\ndata: {\"idx\":0,\"kind\":\"run_started\",\"payload_json\":\"{\\\"run_id\\\":\\\"r-1\\\"}\"}\n\n",
                "id: 1\nevent: agent_message\ndata: {\"idx\":1,\"kind\":\"agent_message\",\"payload_json\":\"{ \\\"text\\\": \\\"hello\\\" }\"}\n\n",
                "event: lagged\ndata: {\"idx\":null,\"kind\":\"lagged\",\"payload_json\":\"{\\\"missed\\\":3}\"}\n\n",
            ),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Harness {
    url: String,
    stub: Arc<Stub>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Harness {
    async fn start() -> Self {
        let stub = Arc::new(Stub::default());
        let app = Router::new().fallback(dispatch).with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{addr}"),
            stub,
        }
    }

    fn seen(&self, needle: &str) -> Option<Seen> {
        self.stub
            .seen
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.path.contains(needle))
            .cloned()
    }

    async fn run(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_engrams"));
        cmd.args(args)
            .env("ENGRAMS_URL", &self.url)
            .env("ENGRAMS_API_KEY", "engk_test-key")
            .envs(env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let stdin = stdin.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let mut child = cmd.spawn().expect("spawn engrams");
            if let Some(text) = stdin {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
            }
            let out = child.wait_with_output().expect("wait engrams");
            Run {
                code: out.status.code().expect("exit code"),
                stdout: String::from_utf8(out.stdout).unwrap(),
                stderr: String::from_utf8(out.stderr).unwrap(),
            }
        })
        .await
        .unwrap()
    }
}

fn tmp_config_dir(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("engrams-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn session_list_json_prints_the_script_stable_shape_and_sends_x_api_key() {
    let h = Harness::start().await;
    let r = h.run(&["--json", "session", "list"], &[], None).await;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let body: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(body["sessions"][0]["id"], "sess-1");
    assert_eq!(body["sessions"][0]["sandbox_id"], "sb-1");
    assert!(
        body["sessions"][0].get("host_id").is_none(),
        "unset optional fields are omitted"
    );
    let rpc = h.seen("ListSessions").expect("ListSessions was called");
    assert_eq!(rpc.api_key.as_deref(), Some("engk_test-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_list_renders_u64_capacities_in_both_output_modes() {
    let h = Harness::start().await;
    let j = h.run(&["--json", "hosts", "list"], &[], None).await;
    assert_eq!(j.code, 0, "stderr: {}", j.stderr);
    let body: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
    assert_eq!(body["hosts"][0]["allocatable_mib"], 16384);
    assert_eq!(body["hosts"][0]["hostname"], "stub-host");
    let t = h.run(&["hosts", "list"], &[], None).await;
    assert_eq!(t.code, 0);
    assert!(t.stdout.contains("16384"), "{}", t.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_credential_exits_1_with_the_auth_login_pointer() {
    let h = Harness::start().await;
    let dir = tmp_config_dir("nocred");
    let r = h
        .run(
            &["session", "list"],
            &[("ENGRAMS_API_KEY", ""), ("XDG_CONFIG_HOME", &dir)],
            None,
        )
        .await;
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("engrams auth login"), "{}", r.stderr);
    assert!(r.stderr.starts_with("engrams: "), "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_exec_streams_stdout_stderr_and_mirrors_a_non_zero_exit() {
    let h = Harness::start().await;
    let r = h.run(&["session", "exec", "sess-1", "ls"], &[], None).await;
    assert_eq!(r.stdout, "out-a\nout-c\n");
    assert!(r.stderr.contains("err-b"));
    // Remote exit 3 → CLI exit 1 (pipeline-composable).
    assert_eq!(r.code, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_exec_json_accumulates_and_reports_exit_status() {
    let h = Harness::start().await;
    let r = h
        .run(&["--json", "session", "exec", "sess-1", "ls"], &[], None)
        .await;
    let body: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(body["stdout"], "out-a\nout-c\n");
    assert_eq!(body["stderr"], "err-b\n");
    assert_eq!(body["exit_status"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_logs_tails_the_sse_route_skipping_pings() {
    let h = Harness::start().await;
    let r = h.run(&["session", "logs", "sess-1"], &[], None).await;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let lines: Vec<&str> = r.stdout.trim().split('\n').collect();
    // The payload, compacted, not the `{idx,kind,payload_json}` envelope.
    assert_eq!(lines[0], r#"[     0] run_started: {"run_id":"r-1"}"#);
    assert_eq!(lines[1], r#"[     1] agent_message: {"text":"hello"}"#);
    assert_eq!(lines[2], r#"[     -] lagged: {"missed":3}"#);
    assert_eq!(lines.len(), 3);
    assert!(!r.stdout.contains("ping"));
}

#[tokio::test(flavor = "multi_thread")]
async fn task_create_resolves_a_profile_by_name_and_prints_the_session_id() {
    let h = Harness::start().await;
    let r = h
        .run(
            &[
                "task",
                "create",
                "--profile",
                "backend",
                "--prompt",
                "fix it",
            ],
            &[],
            None,
        )
        .await;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "sess-for-prof-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn task_create_rejects_an_ambiguous_profile_name() {
    let h = Harness::start().await;
    let r = h
        .run(&["task", "create", "--profile", "dupe"], &[], None)
        .await;
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ambiguous"), "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_login_device_flow_then_cli_key_exchange_then_hosts_json_0600() {
    let h = Harness::start().await;
    let dir = tmp_config_dir("login");
    let env: &[(&str, &str)] = &[
        ("ENGRAMS_API_KEY", ""),
        ("XDG_CONFIG_HOME", &dir),
        ("BROWSER", "true"),
    ];
    // "\n" answers "Press Enter to open the browser…".
    let r = h.run(&["auth", "login"], env, Some("\n")).await;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("copy your one-time code: ABCD1234"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("Logged in to"), "{}", r.stderr);
    let hosts_path = std::path::Path::new(&dir)
        .join("engrams")
        .join("hosts.json");
    let hosts: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&hosts_path).unwrap()).unwrap();
    let cred = &hosts[&h.url];
    assert_eq!(cred["apiKey"], "engk_minted-plaintext");
    assert_eq!(cred["keyId"], "key-1");
    assert_eq!(cred["user"], "alice@example.com");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&hosts_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // The exchange leg authenticated with the device session bearer.
    let exchange = h.seen("CreateCliKey").expect("CreateCliKey was called");
    assert_eq!(
        exchange.authorization.as_deref(),
        Some("Bearer session-token-xyz")
    );

    // The stored key now authenticates normally + logout revokes and forgets.
    let env: &[(&str, &str)] = &[("ENGRAMS_API_KEY", ""), ("XDG_CONFIG_HOME", &dir)];
    let status = h.run(&["auth", "status"], env, None).await;
    assert_eq!(status.code, 0, "stderr: {}", status.stderr);
    assert!(
        status.stdout.contains("alice@example.com"),
        "{}",
        status.stdout
    );
    let logout = h.run(&["auth", "logout"], env, None).await;
    assert_eq!(logout.code, 0, "stderr: {}", logout.stderr);
    let after: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&hosts_path).unwrap()).unwrap();
    assert!(!after.contains_key(&h.url));
    let revoke = h.seen("RevokeCliKey").expect("RevokeCliKey was called");
    assert_eq!(revoke.api_key.as_deref(), Some("engk_minted-plaintext"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_status_json_reports_the_env_var_source() {
    let h = Harness::start().await;
    let r = h.run(&["--json", "auth", "status"], &[], None).await;
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let body: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(body["loggedIn"], true);
    assert_eq!(body["user"], "alice@example.com");
    assert_eq!(body["source"], "ENGRAMS_API_KEY");
}

#[tokio::test(flavor = "multi_thread")]
async fn version_and_usage_errors_follow_the_commander_contract() {
    let h = Harness::start().await;
    let v = h.run(&["--version"], &[], None).await;
    assert_eq!(v.code, 0);
    // The bare release version on its own line (release.yml compares it).
    let parts: Vec<&str> = v.stdout.trim().split('.').collect();
    assert_eq!(parts.len(), 3, "{}", v.stdout);
    assert!(
        parts.iter().all(|p| p.parse::<u32>().is_ok()),
        "{}",
        v.stdout
    );
    assert!(v.stderr.is_empty());
    let bad = h.run(&["session", "list", "--bogus"], &[], None).await;
    assert_eq!(bad.code, 1);
    assert!(bad.stdout.is_empty());
    let none = h.run(&[], &[], None).await;
    assert_eq!(none.code, 1);
    assert!(none.stdout.is_empty());
    assert!(none.stderr.contains("Usage:"));
}
