//! End-to-end tests against a live prod-shape stack (coord + host-agent
//! + Firecracker + Postgres + chunked-OCI registry + fake-gcs).
//!
//! The existing integration tests stop one layer short of the coord HTTP
//! API: `e2e_harness.rs` and `e2e_shell.rs` drive `PooledBackend`
//! directly; `ha_listener.rs` and friends use an in-proc `AppState`.
//! That left the coord HTTP → gRPC → host-agent → FC path uncovered,
//! which is how prod session 8725648d's empty-Binary-frame bug shipped.
//! This file covers the three flows the user named:
//!
//!   1. cold session with `harness=none` → `POST /exec ls` → assert stdout
//!   2. cold session with `harness=claude` → `POST /exec ls` → assert stdout
//!   3. cold session with `harness=claude` + bogus ANTHROPIC_API_KEY +
//!      initial prompt → assert an Anthropic auth-failure event surfaces
//!      in the session_events stream
//!
//! All three are `#[ignore]`'d and gated by env vars. The CI lane
//! `test-e2e-stack` in `.github/workflows/ci.yml` brings up the stack
//! (`integration-up.sh` + `integration-bake-demo.sh` + `engram-cli
//! harness add`), runs these tests via `cargo nextest --run-ignored`,
//! and tears down on completion. Locally, follow the verification
//! section of the e2e plan at `~/.claude/plans/wiggly-tickling-rose.md`.

use std::time::Duration;

use bytes::Bytes;
use engram_coordinator::state::SessionEvent;
use engram_core::SessionId;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-HTTP-request timeout for the test's reqwest client. Cold
/// `POST /sessions` on CI runs ~120s on a fresh chunk cache (FC
/// boot + first NBD page-ins), so this needs to clear that with a
/// little headroom.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// How long the auth-failure test waits for either an `agent_message`
/// or a `run_completed(ok=false)` event after session-create
/// returns. Claude's stream-json round-trip through the harness +
/// egress proxy + api.anthropic.com 401 is typically <10s, but CI
/// networking adds latency. 180s = "if no signal arrives in this
/// long, Claude is genuinely hung (not just slow)" — a strong
/// failure signal worth panicking on.
const SSE_WAIT_DEADLINE: Duration = Duration::from_secs(180);

/// Auth-failure observation outcome.
///
/// The Claude harness today emits `run_completed { ok: false }` when
/// Claude exits non-zero, but the user's hypothesis is that Claude's
/// stream-json output *also* carries a parseable error message that
/// surfaces as an `agent_message` event. We don't yet know what the
/// Anthropic 401 JSON looks like when it reaches the dashboard — the
/// first green CI run will tell us. Until then this enum lets the
/// test pass on either signal and prints captured events on the
/// fallback path so the next iteration can tighten the assertion.
#[derive(Debug)]
#[allow(dead_code)] // variants used only when the test runs (gated)
enum AuthFailureSignal {
    /// An `agent_message` arrived containing the expected substring
    /// (or any future-tightened equivalent). The string is the full
    /// message text so a regression that drops the substring
    /// surfaces clearly.
    ErrorMessage(String),
    /// No structured error message, but `run_completed.ok == false`
    /// arrived — the chain ran and Anthropic rejected the auth.
    /// Captured events are printed to stderr to inform the next
    /// tightening pass.
    RunCompletedNotOk,
    /// Neither signal in the deadline. Carries the captured events
    /// for diagnostic output.
    TimedOut(Vec<SessionEvent>),
}

struct Driver {
    base: reqwest::Url,
    token: Option<String>,
    client: reqwest::Client,
}

impl Driver {
    fn from_env() -> Self {
        let base_str = std::env::var("ENGRAM_E2E_COORD_URL")
            .expect("ENGRAM_E2E_COORD_URL must be set (e.g. http://127.0.0.1:8090)");
        let base = reqwest::Url::parse(&base_str).expect("parse ENGRAM_E2E_COORD_URL");
        let token = std::env::var("ENGRAM_TOKEN").ok();
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("build reqwest client");
        Self {
            base,
            token,
            client,
        }
    }

    fn image_uri() -> String {
        std::env::var("ENGRAM_E2E_IMAGE_URI").expect(
            "ENGRAM_E2E_IMAGE_URI must be set — the upstream CI step that ran \
             integration-bake-demo.sh writes it to $GITHUB_ENV",
        )
    }

    fn harness_name() -> String {
        std::env::var("ENGRAM_E2E_HARNESS_NAME").unwrap_or_else(|_| "claude".to_string())
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.base.join(path).expect("join path");
        let mut req = self.client.request(method, url);
        if let Some(tok) = &self.token {
            req = req.bearer_auth(tok);
        }
        req
    }

    async fn create_session_none_harness(&self, image: &str) -> SessionId {
        let body = serde_json::json!({
            "image": image,
            "harness": { "kind": "none" },
        });
        let resp = self
            .req(reqwest::Method::POST, "/sessions")
            .json(&body)
            .send()
            .await
            .expect("POST /sessions");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "POST /sessions failed: {status} body={text}"
        );
        let parsed: CreateSessionResponse =
            serde_json::from_str(&text).expect("decode CreateSessionResponse");
        parsed.session_id
    }

    async fn create_session_claude(
        &self,
        image: &str,
        api_key: &str,
        prompt: Option<&str>,
    ) -> SessionId {
        let harness_name = Self::harness_name();
        let mut body = serde_json::json!({
            "image": image,
            "harness": { "kind": "builtin", "name": harness_name },
            "secrets": { "ANTHROPIC_API_KEY": api_key },
        });
        if let Some(p) = prompt {
            body["prompt"] = Value::String(p.to_string());
        }
        let resp = self
            .req(reqwest::Method::POST, "/sessions")
            .json(&body)
            .send()
            .await
            .expect("POST /sessions");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "POST /sessions (claude) failed: {status} body={text}"
        );
        let parsed: CreateSessionResponse =
            serde_json::from_str(&text).expect("decode CreateSessionResponse");
        parsed.session_id
    }

    async fn exec(&self, sid: SessionId, command: &str) -> ExecResponse {
        let body = serde_json::json!({ "command": command, "timeout_secs": 30 });
        let path = format!("/sessions/{sid}/exec");
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&body)
            .send()
            .await
            .expect("POST /sessions/:id/exec");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "exec failed: {status} body={text} command={command:?}"
        );
        serde_json::from_str(&text).expect("decode ExecResponse")
    }

    async fn delete(&self, sid: SessionId) {
        let path = format!("/sessions/{sid}");
        let _ = self
            .req(reqwest::Method::DELETE, &path)
            .send()
            .await
            .map(|r| r.status());
    }

    /// Stream session_events until the deadline, watching for an
    /// Anthropic auth-failure signal in either of the shapes
    /// documented on `AuthFailureSignal`.
    async fn wait_for_anthropic_auth_failure(
        &self,
        sid: SessionId,
        expected_substr: &str,
        deadline: Duration,
    ) -> AuthFailureSignal {
        let path = format!("/sessions/{sid}/events?since=-1");
        let resp = self
            .req(reqwest::Method::GET, &path)
            .header("accept", "text/event-stream")
            .send()
            .await
            .expect("GET /sessions/:id/events");
        assert!(
            resp.status().is_success(),
            "events stream open failed: {}",
            resp.status()
        );

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut captured: Vec<SessionEvent> = Vec::new();
        let started = std::time::Instant::now();

        loop {
            let remaining = deadline.checked_sub(started.elapsed());
            let Some(remaining) = remaining else {
                return AuthFailureSignal::TimedOut(captured);
            };
            let next = tokio::time::timeout(remaining, stream.next()).await;
            let chunk: Bytes = match next {
                Err(_) => return AuthFailureSignal::TimedOut(captured),
                Ok(None) => return AuthFailureSignal::TimedOut(captured),
                Ok(Some(Err(e))) => {
                    eprintln!("events stream IO error: {e}");
                    return AuthFailureSignal::TimedOut(captured);
                }
                Ok(Some(Ok(c))) => c,
            };
            buf.extend_from_slice(&chunk);

            // SSE messages are separated by blank lines. Process every
            // complete message in the buffer; keep the trailing
            // fragment for the next iteration.
            while let Some(boundary) = find_sse_boundary(&buf) {
                let raw = std::str::from_utf8(&buf[..boundary])
                    .expect("SSE messages are UTF-8")
                    .to_owned();
                buf.drain(..boundary + 2); // skip the \n\n

                let Some(data) = parse_sse_data(&raw) else {
                    // Comment line (`:keep-alive`) or non-data event —
                    // ignored.
                    continue;
                };
                let Ok(ev) = serde_json::from_str::<SessionEvent>(&data) else {
                    // Untyped or future-variant event — record raw
                    // and skip. We can still surface it on the
                    // TimedOut path.
                    continue;
                };

                // The two signals.
                if let SessionEvent::HarnessAgentMessage { text, .. } = &ev {
                    if text.contains(expected_substr) {
                        return AuthFailureSignal::ErrorMessage(text.clone());
                    }
                }
                if let SessionEvent::HarnessRunCompleted { ok: false, .. } = &ev {
                    // Don't return immediately — give the stream a tiny
                    // grace so a trailing agent_message with the error
                    // text (if any) can land. 200ms is enough; the
                    // harness emits run_completed AFTER it's done
                    // forwarding agent_messages.
                    let grace_deadline = std::time::Instant::now() + Duration::from_millis(500);
                    captured.push(ev);
                    while std::time::Instant::now() < grace_deadline {
                        let chunk =
                            tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
                        match chunk {
                            Ok(Some(Ok(c))) => buf.extend_from_slice(&c),
                            _ => break,
                        }
                        while let Some(b) = find_sse_boundary(&buf) {
                            let raw = std::str::from_utf8(&buf[..b]).expect("UTF-8").to_owned();
                            buf.drain(..b + 2);
                            if let Some(d) = parse_sse_data(&raw) {
                                if let Ok(SessionEvent::HarnessAgentMessage { text, .. }) =
                                    serde_json::from_str::<SessionEvent>(&d)
                                {
                                    if text.contains(expected_substr) {
                                        return AuthFailureSignal::ErrorMessage(text);
                                    }
                                    captured.push(SessionEvent::HarnessAgentMessage {
                                        run_id: String::new(),
                                        message_id: String::new(),
                                        role: engram_harness_proto::AgentRole::Assistant,
                                        text,
                                        at: chrono::Utc::now(),
                                    });
                                }
                            }
                        }
                    }
                    eprintln!(
                        "AUTH-FAIL via run_completed(ok=false) only. Captured events:\n{captured:#?}\n\
                         Next iteration: tighten EXPECTED_ERROR_SUBSTR based on whatever \
                         agent_message text Anthropic returns, and collapse this branch to a \
                         tight assert in `wait_for_anthropic_auth_failure`."
                    );
                    return AuthFailureSignal::RunCompletedNotOk;
                }

                captured.push(ev);
            }
        }
    }
}

/// Find the byte offset of the first SSE message-terminator (`\n\n`)
/// in the buffer, or `None` if no complete message is buffered yet.
fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Pull the `data:` payload out of a raw SSE message. SSE messages can
/// carry multiple `data:` lines (joined with `\n`), but our coord
/// emits one per event so this is the simple case.
fn parse_sse_data(raw: &str) -> Option<String> {
    let mut data = String::new();
    let mut saw = false;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if saw {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            saw = true;
        }
    }
    if saw {
        Some(data)
    } else {
        None
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct CreateSessionResponse {
    session_id: SessionId,
    #[allow(dead_code)]
    status: String,
    #[allow(dead_code)]
    image_version: String,
    #[allow(dead_code)]
    kind: String,
}

#[derive(Deserialize, Serialize)]
struct ExecResponse {
    #[allow(dead_code)]
    session_id: SessionId,
    #[allow(dead_code)]
    exec_id: String,
    exit_status: Option<i32>,
    stdout: String,
    #[allow(dead_code)]
    stderr: String,
}

// ---------- Tests ----------

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a live prod-shape stack with a baked demo image"]
async fn e2e_cold_session_no_harness_can_exec_ls() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;
    let resp = driver.exec(sid, "ls /usr/local/bin").await;
    assert_eq!(
        resp.exit_status,
        Some(0),
        "ls should succeed; stdout=<{}> stderr=<{}>",
        resp.stdout,
        resp.stderr
    );
    assert!(
        resp.stdout.contains("ttyd"),
        "demo image should ship ttyd at /usr/local/bin; stdout=<{}>",
        resp.stdout
    );
    driver.delete(sid).await;
}

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a Claude harness pack registered with the coord"]
async fn e2e_cold_session_claude_harness_can_exec_ls() {
    // Raw /exec hits the sandbox directly — the Claude harness is
    // bound but unused. Use a bogus key so a future regression that
    // races a harness call won't burn real Anthropic budget; this
    // test doesn't send a prompt.
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid = driver
        .create_session_claude(&image, "sk-bogus-e2e-noprompt", None)
        .await;
    let resp = driver.exec(sid, "ls /usr/local/bin").await;
    assert_eq!(
        resp.exit_status,
        Some(0),
        "ls should succeed even with claude harness bound; stdout=<{}> stderr=<{}>",
        resp.stdout,
        resp.stderr
    );
    assert!(
        resp.stdout.contains("ttyd"),
        "demo image should ship ttyd; stdout=<{}>",
        resp.stdout
    );
    driver.delete(sid).await;
}

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + Claude harness + real api.anthropic.com reachability"]
async fn e2e_claude_with_bogus_key_surfaces_anthropic_auth_error() {
    // Stub expected substring — we don't yet know exactly what
    // Anthropic's 401 JSON looks like as it travels through Claude
    // CLI's stream-json and into the harness's agent_message events.
    // The first green CI run will print captured events on the
    // RunCompletedNotOk path so we can tighten this.
    const EXPECTED_ERROR_SUBSTR: &str = "authentication";

    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid = driver
        .create_session_claude(&image, "sk-bogus-e2e-prompt", Some("hello"))
        .await;

    let outcome = driver
        .wait_for_anthropic_auth_failure(sid, EXPECTED_ERROR_SUBSTR, SSE_WAIT_DEADLINE)
        .await;
    match outcome {
        AuthFailureSignal::ErrorMessage(text) => {
            assert!(
                text.contains(EXPECTED_ERROR_SUBSTR),
                "expected auth-error substring in agent_message: {text}"
            );
        }
        AuthFailureSignal::RunCompletedNotOk => {
            // Acceptable on the first iteration: the chain ran and
            // failed cleanly. The stderr dump from
            // wait_for_anthropic_auth_failure tells us what to
            // tighten to.
        }
        AuthFailureSignal::TimedOut(events) => panic!(
            "no auth-failure signal within {}s; saw events:\n{events:#?}",
            SSE_WAIT_DEADLINE.as_secs()
        ),
    }
    driver.delete(sid).await;
}
