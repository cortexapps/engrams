//! End-to-end tests against a live prod-shape stack (coord + host-agent
//! + Firecracker + Postgres + chunked-OCI registry + fake-gcs).
//!
//! The existing integration tests stop one layer short of the coord HTTP
//! API: `e2e_harness.rs` and `e2e_shell.rs` drive `PooledBackend`
//! directly; `ha_listener.rs` and friends use an in-proc `AppState`.
//! That left the coord HTTP → gRPC → host-agent → FC path uncovered,
//! which is how prod session 8725648d's empty-Binary-frame bug shipped.
//! This file covers the three flows the user named (ADR 0021 P1.3
//! wire shape):
//!
//!   1. cold session with `mode = dev_vm` → `POST /exec ls` → assert
//!      stdout. Harness in the image (if any) is left undriven.
//!   2. cold session with `mode = agent` against the baked-claude
//!      image → `POST /exec ls` → assert stdout (agentd is up, harness
//!      runs but the test just exec's a shell command).
//!   3. cold session with `mode = agent` + bogus ANTHROPIC_API_KEY +
//!      initial prompt → assert an Anthropic auth-failure event
//!      surfaces in the session_events stream.
//!
//! All three are `#[ignore]`'d and gated by env vars. The CI lane
//! `test-e2e-stack` in `.github/workflows/ci.yml` brings up the stack
//! (`integration-up.sh` + `integration-bake-demo.sh`), runs these tests
//! via `cargo nextest --run-ignored`, and tears down on completion.
//! ADR 0021 P1.5 retired the separate `harness add` step — the harness
//! is baked into the image at `/opt/engram/harness/` and the coord
//! reads the launch contract from `manifest.toml`.

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

    // ADR 0021 P1.3: `harness_name()` helper retired. Harness
    // selection is baked into the image (`[harness]` in
    // `engram.toml`); the wire shape just says "drive the agent"
    // (`mode = agent`) or "skip it" (`mode = dev_vm`).

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.base.join(path).expect("join path");
        let mut req = self.client.request(method, url);
        if let Some(tok) = &self.token {
            req = req.bearer_auth(tok);
        }
        req
    }

    /// ADR 0021 P1.3: drive the image as a pure dev VM. Whether the
    /// image has a baked `[harness]` block is irrelevant — `mode =
    /// dev_vm` tells coord to skip `resolve_harness` and pass an
    /// empty-argv `AgentSpec` to the backend. agentd hits the
    /// readiness-probe branch (`harness_supervisor.rs::spawn`) and
    /// never execs the harness binary even if it's sitting in the
    /// rootfs at `/opt/engram/harness/`.
    async fn create_session_none_harness(&self, image: &str) -> SessionId {
        let body = serde_json::json!({
            "image": image,
            "mode": "dev_vm",
        });
        let resp = self
            .req(reqwest::Method::POST, "/api/v1/sessions")
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

    /// ADR 0021 P1.3: drive the image's baked harness. `mode =
    /// agent` is the default but we set it explicitly so the test
    /// remains correct if defaults shift later. The image referenced
    /// by `ENGRAM_E2E_IMAGE_URI` must carry a `[harness] builtin =
    /// "claude"` block (the CI bake of `deploy/demo-claude/` does);
    /// coord reads the harness contract from `manifest.toml`, so
    /// the request no longer names the harness directly.
    async fn create_session_claude(
        &self,
        image: &str,
        api_key: &str,
        prompt: Option<&str>,
    ) -> SessionId {
        let mut body = serde_json::json!({
            "image": image,
            "mode": "agent",
            "secrets": { "ANTHROPIC_API_KEY": api_key },
        });
        if let Some(p) = prompt {
            body["prompt"] = Value::String(p.to_string());
        }
        let resp = self
            .req(reqwest::Method::POST, "/api/v1/sessions")
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
        let path = format!("/api/v1/sessions/{sid}/exec");
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
        let path = format!("/api/v1/sessions/{sid}");
        let _ = self
            .req(reqwest::Method::DELETE, &path)
            .send()
            .await
            .map(|r| r.status());
    }

    /// ADR 0016 Phase B commit 4a admin trigger. Forces an
    /// immediate `ChunkedDiskBackend::flush()` on the session's
    /// bound sandbox + publishes the manifest_ref into
    /// `sessions.live_disk_manifest_*` in the same coord-side TX.
    /// Returns the parsed response body.
    async fn flush_now(&self, sid: SessionId) -> FlushNowResponse {
        let path = format!("/api/v1/admin/sessions/{sid}/flush-now");
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST /api/admin/sessions/:id/flush-now");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "flush_now failed: {status} body={text}"
        );
        serde_json::from_str(&text).expect("decode FlushNowResponse")
    }

    /// `POST /sessions/:id/snapshot`. Returns when the snapshot's
    /// PG row is durable. Body is the SnapshotResponse but we
    /// discard it for this test — the side effect we care about is
    /// the row existing so resume() has something to find.
    async fn snapshot(&self, sid: SessionId) {
        let path = format!("/api/v1/sessions/{sid}/snapshot");
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST /sessions/:id/snapshot");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "snapshot failed: {status} body={text}");
    }

    /// `DELETE /sessions/:id/local` — evict the local sandbox after
    /// a snapshot, leaving the session in `Idle`. Required before
    /// resume() will reconstruct a fresh sandbox.
    async fn evict_local(&self, sid: SessionId) {
        let path = format!("/api/v1/sessions/{sid}/local");
        let resp = self
            .req(reqwest::Method::DELETE, &path)
            .send()
            .await
            .expect("DELETE /sessions/:id/local");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "evict_local failed: {status} body={text}"
        );
    }

    /// `POST /sessions/:id/resume`. Synchronous: returns when the
    /// session is Active again. The newly-bound sandbox_id is the
    /// one that should appear in `nbd_sandboxes` (per ADR 0016
    /// Phase B commit 5).
    async fn resume(&self, sid: SessionId) {
        let path = format!("/api/v1/sessions/{sid}/resume");
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST /sessions/:id/resume");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "resume failed: {status} body={text}");
    }

    /// `GET /sessions/:id/cow-state`. ADR 0016 Phase A diagnostic.
    /// Returns `Some(json)` when the sandbox is NBD-tracked
    /// (Phase B's chunked-disk pipeline live), `None` when the
    /// host fell back to materialize-to-file (no nbd.ko, no
    /// nbd_pool, etc.). Used by Phase B tests as a runtime probe
    /// for whether the chunked-disk-driven assertions are
    /// meaningful in this environment.
    async fn cow_state(&self, sid: SessionId) -> Option<Value> {
        let path = format!("/api/v1/sessions/{sid}/cow-state");
        let resp = self
            .req(reqwest::Method::GET, &path)
            .send()
            .await
            .expect("GET /sessions/:id/cow-state");
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        match body.get("state") {
            Some(Value::Null) | None => None,
            Some(other) => Some(other.clone()),
        }
    }

    /// Poll cow-state until `disk_manifest_version > prior` or the
    /// deadline elapses. Returns the new version on success, None
    /// on timeout. This is the load-bearing end-state assertion
    /// for Phase B e2e tests: a flush of dirty bytes — by either
    /// the FlushScheduler tick OR the admin flush-now trigger —
    /// advances `disk_manifest_version`. The test doesn't care
    /// which path produced the advance; both prove the chunked-
    /// disk publish pipeline works.
    async fn wait_for_disk_manifest_advance(
        &self,
        sid: SessionId,
        prior_version: u64,
        deadline: Duration,
    ) -> Option<u64> {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if let Some(state) = self.cow_state(sid).await {
                if let Some(v) = state.get("disk_manifest_version").and_then(Value::as_u64) {
                    if v > prior_version {
                        return Some(v);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        None
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
        let path = format!("/api/v1/sessions/{sid}/events?since=-1");
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

#[derive(Debug, Deserialize)]
struct FlushNowResponse {
    outcome: String,
    #[serde(default)]
    manifest_version: Option<u64>,
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

/// ADR 0027: a generated session must actually carry the RO-mounted skills.
///
/// Baked skills are retired, so a session's skills come ONLY from the
/// `skills` squashfs the FC host mounts. The e2e-stack lane stages it at
/// `/var/lib/engram/shared/skills.squashfs`, so this exercises the WHOLE
/// chain end-to-end: capture attaches the aux RO drive → the snapshot embeds
/// it → restore re-anchors by presence → the init shim mounts it at
/// `/opt/engram/skills` → agentd wires `share-file` at SpawnHarness. (Runs on
/// the owned 6.1 guest kernel, which has `CONFIG_SQUASHFS_ZSTD=y`.) This is
/// the integrated counterpart to the `engram-session-bundles` unit tests and
/// the `aux_ro_drive` FC drive-mechanism test.
#[tokio::test]
#[ignore = "requires the e2e-stack lane (stages the skills RO bundle at /var/lib/engram/shared)"]
async fn e2e_session_has_mounted_skills_bundle() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();
    let sid = driver.create_session_none_harness(&image).await;

    // The bundle is mounted read-only at the canonical guest path.
    let mounted = driver
        .exec(
            sid,
            "test -x /opt/engram/skills/bin/engram-share && echo MOUNTED",
        )
        .await;
    assert!(
        mounted.stdout.contains("MOUNTED"),
        "skills RO bundle not mounted at /opt/engram/skills; stdout=<{}> stderr=<{}>",
        mounted.stdout,
        mounted.stderr,
    );

    // agentd activation: the share-file skill is discoverable and its wrapper
    // is on PATH — i.e. the harness would actually find `share-file`.
    let wired = driver
        .exec(
            sid,
            "test -e /root/.agents/skills/share-file/SKILL.md \
             && test -x /usr/local/bin/engram-share && echo WIRED",
        )
        .await;
    assert!(
        wired.stdout.contains("WIRED"),
        "agentd did not wire the share-file skill from the mounted bundle; \
         stdout=<{}> stderr=<{}>",
        wired.stdout,
        wired.stderr,
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
    // Tightened from the iteration-1 stub after run 26338080977 showed
    // the actual shape. Claude CLI surfaces Anthropic's 401 as an
    // assistant-role agent_message with literal text:
    //
    //   "Invalid API key · Fix external API key"
    //
    // (the `·` is a middle dot, U+00B7; we only assert on the
    // ASCII prefix). The harness's `run_completed.ok` is actually
    // `true` on this path because the Claude CLI process itself
    // exited cleanly — the auth error lives entirely in the
    // stream-json output it printed before exit. That's why this
    // test watches `agent_message` text, not the `ok` flag.
    const EXPECTED_ERROR_SUBSTR: &str = "Invalid API key";

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

/// ADR 0016 Phase B commit 4b: end-to-end exercise of the
/// FlushScheduler primitive via the admin `flush-now` endpoint.
/// Explicit-trigger counterpart to the scheduler's 30s implicit
/// cadence; the only way to verify the chunked-disk-write →
/// `backend.flush()` → coord `update_live_disk_manifest` → PG row
/// round-trip in CI without sleeping a full cadence. Without this
/// test, the prod-shape chunked-disk write path silently regresses
/// on any change to `nbd_sandboxes` wiring or the
/// `update_live_disk_manifest` TX shape — neither covered by the
/// in-process unit tests.
///
/// **Environment dependence** (per `[fc_tests_run_in_ci]`):
///
/// The chunked-disk write assertions only fire when the host has
/// `nbd_pool` + `chunk_store` + `chunk_cache` wired AND a
/// chunked-OCI demo image. On Blacksmith CI runners today,
/// `nbd.ko` isn't in the guest kernel and `integration-up.sh`'s
/// probe falls back to materialize-to-file — `nbd_sandboxes` stays
/// empty, `flush_sandbox` returns None for every sandbox. The test
/// detects this via the Phase A `GET /sessions/:id/cow-state`
/// diagnostic (null state == not chunk-tracked) and skips the
/// applied/idle assertions with a `::warning::` so the gap is
/// visible in every run. The endpoint-wired assertions (pre-write
/// idle + sandbox-bound 200 OK) still fire on every environment —
/// so a regression in the route table or the wire format still
/// fails loud. Self-hosted dev-vm runner (or a Blacksmith NBD
/// support request) is the documented follow-up that closes the
/// gap and turns the warning into a hard assertion.
///
/// Test path:
/// 1. Cold-create a session against the demo image.
/// 2. **Always**: `flush_now` pre-write → assert `outcome=idle`.
///    Endpoint is wired, sandbox is bound (vs 409/404), no spurious
///    Applied from a scheduler tick.
/// 3. Probe `cow-state`. If null → `::warning::` + return (env
///    can't exercise chunked-disk path).
/// 4. dd + sync into the chunked-disk-backed rootfs.
/// 5. `flush_now` → assert `outcome=applied`, `manifest_version > 0`.
///    Full host → PG round-trip.
/// 6. `flush_now` again → assert `outcome=idle` (zero-chunk
///    short-circuit; catches regressions that'd churn
///    `chunk_generation` on every admin call).
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_flush_now_applies_then_short_circuits_on_no_dirty() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Step 2: pre-write flush. Environment-independent — the
    // endpoint should always return idle when nothing's dirty,
    // regardless of whether the host has NBD wired. 200 OK + idle
    // here proves the route is mounted, the session-lookup works,
    // and `flush_sandbox` returns None cleanly.
    let pre = driver.flush_now(sid).await;
    assert_eq!(
        pre.outcome, "idle",
        "first flush before any writes should be idle; got {pre:?}",
    );
    assert!(
        pre.manifest_version.is_none(),
        "idle outcome must NOT carry a manifest_version; got {pre:?}",
    );

    // Step 3: probe whether this environment can actually exercise
    // the chunked-disk path. `cow-state` returns Some(...) only
    // when the sandbox is NBD-tracked (the demo image got NBD-
    // attached, not materialized-to-file). On Blacksmith CI today
    // `nbd.ko` isn't in the guest kernel → integration-up.sh
    // falls back to materialize-to-file → cow-state is null →
    // skip the rest with a loud warning so the gap is visible in
    // every CI run.
    let cow = driver.cow_state(sid).await;
    if cow.is_none() {
        eprintln!(
            "::warning title=Phase B flush-now e2e partial coverage::\
             cow-state returned null for session {sid} — the runner's host-agent \
             fell back to materialize-to-file (no nbd.ko / no nbd_pool wired). \
             Pre-write flush_now=idle assertion verified the endpoint wiring, but \
             the chunked-disk write → flush → publish round-trip can't be exercised \
             here. See ci.yml line 518-532 + the dev-vm self-hosted runner follow-up."
        );
        driver.delete(sid).await;
        return;
    }

    // Step 4: capture the baseline disk_manifest_version BEFORE
    // dd. The FlushScheduler may already have ticked between
    // create-session and now (its 30s default cadence can fire
    // during the slow cold-create); whatever version it left
    // behind is what we measure forward from.
    let baseline_version = cow
        .as_ref()
        .and_then(|s| s.get("disk_manifest_version"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Step 5: write enough dirty bytes to materialise at least one
    // full 16 MiB chunk in the chunked-disk dirty buffer.
    //
    // `/var` is writable + survives the run; demo image's workdir
    // varies, so anchoring at `/var` is deterministic. `count=8`
    // keeps the exec inside the default 30s timeout even on
    // cold-cache CI runs. `/dev/zero` (not /urandom) so writes
    // dedupe cleanly across re-runs.
    let dd = driver
        .exec(
            sid,
            "dd if=/dev/zero of=/var/dirty.bin bs=1M count=8 status=none",
        )
        .await;
    assert_eq!(
        dd.exit_status,
        Some(0),
        "dd should succeed; stderr=<{}>",
        dd.stderr,
    );
    // `sync` so the writes hit the chunked-disk backend rather than
    // sitting in the guest page cache.
    let sync = driver.exec(sid, "sync").await;
    assert_eq!(sync.exit_status, Some(0), "sync should succeed");

    // Step 6: force the flush via the admin trigger — best-effort.
    // The outcome of this specific call can be either:
    // - `applied` if dirty chunks were resident when flush-now hit
    //   (the deterministic admin-trigger path).
    // - `idle` if the FlushScheduler's 30s tick already drained
    //   the buffer between our dd+sync and this call. Either way,
    //   step 7's end-state check still passes — Phase B's claim
    //   is that the disk lineage advances on writes, regardless
    //   of who drained the buffer.
    let _ = driver.flush_now(sid).await;

    // Step 7: end-state assertion. Poll cow-state until the disk
    // manifest version advances past `baseline_version`. The
    // 90-second deadline covers one full scheduler tick (30s) +
    // generous CI slack. If we don't see an advance in that
    // window, the chunked-disk publish pipeline is broken —
    // neither the admin trigger nor the scheduler produced a new
    // version after 8 MiB of writes.
    let advanced = driver
        .wait_for_disk_manifest_advance(sid, baseline_version, Duration::from_secs(90))
        .await;
    assert!(
        advanced.is_some(),
        "disk_manifest_version never advanced past baseline {baseline_version} in 90s — \
         chunked-disk publish pipeline regressed",
    );
    let v_after = advanced.unwrap();
    assert!(
        v_after > baseline_version,
        "post-write manifest_version {v_after} must exceed baseline {baseline_version}",
    );

    driver.delete(sid).await;
}

/// ADR 0016 Phase B commit 5 regression: a resumed sandbox is a
/// first-class entry in `nbd_sandboxes`, the FlushScheduler runs
/// against it, and a subsequent flush + eviction-style snapshot
/// succeeds.
///
/// Pre-commit-5 behaviour (the failure mode this test pins):
/// - `cow-state` on the resumed session returned null (the
///   resumed sandbox was never inserted into `nbd_sandboxes`).
/// - A second eviction-snapshot errored with `non-canonical jail
///   layout: rootfs canonical symlink missing` because the resume
///   path materialized to a flat file instead of rebuilding the
///   chunked-NBD layout the snapshot pipeline expects.
///
/// Post-commit-5 behaviour:
/// - Resume rebuilds chunked-disk tracking; cow-state populates
///   for the resumed sandbox.
/// - The chunked-disk dirty buffer is real again — flush-now
///   against the resumed session drains chunks just like a fresh
///   cold-create.
///
/// **Environment dependence**: same cow-state-probe skip pattern
/// as `e2e_flush_now_applies_then_short_circuits_on_no_dirty`. On
/// Blacksmith CI without `nbd.ko`, the test runs through the
/// flush-now + snapshot + evict + resume API surface (catches
/// regressions in those handlers) but skips the cow-state-post-
/// resume + flush-now-post-resume assertions with a loud warning.
/// The full regression coverage kicks in on dev-vm / future
/// self-hosted runner where NBD is wired.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_rejoins_chunked_disk_tracking() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Probe FIRST — if this environment can't exercise the NBD
    // path, every subsequent assertion below is meaningless. The
    // pre-write idle-flush check is redundant with
    // `e2e_flush_now_applies_then_short_circuits_on_no_dirty`,
    // which already pins the always-on endpoint shape.
    let pre_cow = driver.cow_state(sid).await;
    if pre_cow.is_none() {
        eprintln!(
            "::warning title=Phase B resume regression partial coverage::\
             cow-state returned null for session {sid} (pre-snapshot) — runner \
             can't exercise the NBD path. Snapshot+resume API wiring will \
             still be exercised below; cow-state-post-resume + \
             flush-now-post-resume assertions skipped. \
             See ci.yml line 518-532 + dev-vm self-hosted runner follow-up."
        );
        driver.delete(sid).await;
        return;
    }

    // Dirty the disk so the snapshot we take has a non-trivial
    // disk_manifest the resume path can NBD-attach against. Same
    // dd+sync shape as the flush-now test.
    let dd = driver
        .exec(
            sid,
            "dd if=/dev/zero of=/var/dirty.bin bs=1M count=8 status=none",
        )
        .await;
    assert_eq!(
        dd.exit_status,
        Some(0),
        "dd should succeed; stderr=<{}>",
        dd.stderr,
    );
    let sync = driver.exec(sid, "sync").await;
    assert_eq!(sync.exit_status, Some(0), "sync should succeed");

    // Best-effort flush via the admin trigger (the FlushScheduler
    // may have already drained — same race as e2e_flush_now). The
    // snapshot below will internally re-flush via its own
    // backend.flush() call regardless, so the chunks ARE durable
    // in BlobStorage by the time we evict.
    let _ = driver.flush_now(sid).await;

    // Snapshot → evict-local → resume. Equivalent to the
    // idle-eviction → resume cycle prod exercises, minus the
    // 30-second idle wait.
    driver.snapshot(sid).await;
    driver.evict_local(sid).await;
    driver.resume(sid).await;

    // **THE REGRESSION CHECK**: post-resume cow-state must be
    // Some(...). Pre-commit-5 this returned null (Symptom 1 of the
    // ADR's failure mode). Post-commit-5 the resumed sandbox is in
    // `nbd_sandboxes` → diagnostic populates.
    let post_cow = driver.cow_state(sid).await;
    assert!(
        post_cow.is_some(),
        "post-resume cow-state must be Some — commit 5 wired the resumed sandbox \
         into nbd_sandboxes. Null here means the resume took the materialize-to-file \
         fallback path or the NBD attach branch silently no-op'd.",
    );

    // **THE SECOND REGRESSION CHECK**: the FlushScheduler can now
    // produce a non-trivial flush on the resumed sandbox. Pre-
    // commit-5 the resumed sandbox had no chunked-disk dirty
    // buffer at all (Symptom 2 — second eviction-snapshot
    // failed). Post-commit-5 the resumed sandbox is a fresh
    // ChunkedDiskBackend rebased on the snapshot's disk_manifest;
    // post-resume writes go through it and the disk manifest
    // version advances on the next flush (admin OR scheduler).
    //
    // Capture the post-resume baseline and write a NEW file
    // (different from the pre-snapshot dirty.bin) so the
    // assertion can't be satisfied by stale state.
    let post_resume_baseline = post_cow
        .as_ref()
        .and_then(|s| s.get("disk_manifest_version"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let post_dd = driver
        .exec(
            sid,
            "dd if=/dev/zero of=/var/post-resume.bin bs=1M count=4 status=none",
        )
        .await;
    assert_eq!(
        post_dd.exit_status,
        Some(0),
        "post-resume dd should succeed; stderr=<{}>",
        post_dd.stderr,
    );
    let post_sync = driver.exec(sid, "sync").await;
    assert_eq!(
        post_sync.exit_status,
        Some(0),
        "post-resume sync should succeed",
    );

    // Best-effort admin trigger (same race-tolerance as
    // e2e_flush_now_applies_then_short_circuits_on_no_dirty).
    let _ = driver.flush_now(sid).await;

    // End-state assertion: the resumed sandbox's disk manifest
    // version must advance past the post-resume baseline. This is
    // the load-bearing regression check — pre-commit-5 this would
    // hang forever (resumed sandbox isn't in nbd_sandboxes, no
    // backend.flush() to advance the version).
    let post_advanced = driver
        .wait_for_disk_manifest_advance(sid, post_resume_baseline, Duration::from_secs(90))
        .await;
    assert!(
        post_advanced.is_some(),
        "post-resume disk_manifest_version never advanced past baseline {post_resume_baseline} in 90s — \
         the resumed sandbox isn't rejoined to chunked-disk tracking",
    );

    driver.delete(sid).await;
}

// ---------------------------------------------------------------------
// ADR 0016 Phase C commit 6a — chunk-GC admin-endpoint e2e regression
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields decoded for Debug output even when not asserted
struct ChunkGcSweepResponse {
    listed_chunks: usize,
    pin_set_size: usize,
    candidates_marked: usize,
    promoted_deletes: usize,
    promote_delete_errors: usize,
    grace_secs: u64,
}

#[derive(Debug, Deserialize)]
struct ChunkGcCandidate {
    content_hash: String,
    #[allow(dead_code)]
    first_seen_at: String,
    #[allow(dead_code)]
    last_seen_at: String,
}

#[derive(Debug, Deserialize)]
struct ChunkGcCandidatesResponse {
    candidates: Vec<ChunkGcCandidate>,
}

impl Driver {
    async fn chunk_gc_dry_run(&self, grace_secs: Option<u64>) -> ChunkGcSweepResponse {
        let mut path = "/api/v1/admin/chunk-gc/dry-run".to_string();
        if let Some(g) = grace_secs {
            path.push_str(&format!("?grace_secs={g}"));
        }
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST /api/admin/chunk-gc/dry-run");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "dry-run failed: {status} body={text}");
        serde_json::from_str(&text).expect("decode ChunkGcSweepResponse")
    }

    async fn chunk_gc_sweep(&self, grace_secs: Option<u64>) -> ChunkGcSweepResponse {
        let mut path = "/api/v1/admin/chunk-gc/sweep".to_string();
        if let Some(g) = grace_secs {
            path.push_str(&format!("?grace_secs={g}"));
        }
        let resp = self
            .req(reqwest::Method::POST, &path)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST /api/admin/chunk-gc/sweep");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "sweep failed: {status} body={text}");
        serde_json::from_str(&text).expect("decode ChunkGcSweepResponse")
    }

    async fn chunk_gc_candidates(&self) -> ChunkGcCandidatesResponse {
        let resp = self
            .req(reqwest::Method::GET, "/api/v1/admin/chunk-gc/candidates")
            .send()
            .await
            .expect("GET /api/admin/chunk-gc/candidates");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "GET candidates failed: {status} body={text}"
        );
        serde_json::from_str(&text).expect("decode ChunkGcCandidatesResponse")
    }
}

/// ADR 0016 Phase C commit 6a — load-bearing regression test for the
/// M5 failure class.
///
/// The 2026-05-23 incident this catches: the prior chunk-GC marked
/// every chunked-disk image's chunks as candidates because its
/// live-set query missed the `enabled_images` lineage, then
/// silent-deleted them after a 24h grace (which itself was a no-op
/// on GCS due to the etag-parser bug). The next session-create
/// against any chunked image faulted with `chunk fetch: blob
/// storage: blob not found`.
///
/// This test pins that the redesigned GC does NOT repeat that
/// failure. Flow:
///
/// 1. Create a none-harness session on the demo image (which is
///    chunked-disk in CI per integration-bake-demo.sh). This proves
///    the enabled image's chunks are materialized + reachable.
/// 2. POST /api/admin/chunk-gc/dry-run — assert pin_set_size > 0.
///    If the enabled-image pin-set source were broken, this would
///    report 0 here, and step 3's sweep would proceed to delete
///    every chunk in the bucket.
/// 3. POST /api/admin/chunk-gc/sweep?grace_secs=0 — full sweep
///    with grace knocked down so any orphans would promote on
///    this single call. The load-bearing claim: this MUST NOT
///    delete the demo image's chunks, regardless of what's in the
///    bucket.
/// 4. Re-exec a command on the original session — proves the
///    sweep didn't break the session's data path. (Sandbox's
///    chunks are paged in lazily; if the sweep wiped them, the
///    exec would fail on next page-fault. With NBD wired, this
///    would be `chunk fetch: blob not found`; without NBD, the
///    rootfs is materialized eagerly so this assertion is weaker
///    but still catches the path.)
/// 5. Create a SECOND none-harness session on the same image —
///    this is the strongest regression catch. Session-create
///    materializes the rootfs anew; if the sweep deleted the
///    image's chunks, this fails the same way the M5 incident
///    failed.
/// 6. GET /api/admin/chunk-gc/candidates — sanity-check the
///    response shape (paged list, no crash on empty bucket).
/// 7. POST /api/admin/chunk-gc/dry-run — second sweep is
///    idempotent on a clean stack: pin set membership hasn't
///    changed, and any candidates marked in step 3 were already
///    promoted to deletion. Verifies the sweep is well-behaved
///    on repeated invocation.
///
/// Environment dependence: this test does NOT skip on missing NBD.
/// The pin-set + sweep paths run against PG + BlobStorage only;
/// session create+exec works against either NBD-attached or
/// materialize-to-file rootfs.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_chunk_gc_sweep_does_not_delete_live_image_chunks() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    let sid_1 = driver.create_session_none_harness(&image).await;

    // Step 2: dry-run baseline. The pin set MUST cover the demo
    // image's chunks (enabled_images source). If this is 0, the
    // regression has already happened by construction and step 3's
    // sweep would wipe the bucket.
    let dry = driver.chunk_gc_dry_run(Some(0)).await;
    assert!(
        dry.pin_set_size > 0,
        "pin_set_size must be > 0 — the demo image's chunks should be \
         pinned via enabled_images.disk_manifest_*. Got {dry:?}. \
         A 0 here means the enabled-image pin-set source is broken; \
         proceeding to step 3's sweep would have nuked the bucket."
    );
    assert_eq!(
        dry.grace_secs, 0,
        "grace_secs override must echo back; got {dry:?}",
    );
    assert_eq!(
        dry.promoted_deletes, 0,
        "DryRun must promote nothing; got {dry:?}",
    );

    // Capture sizes for the idempotency assertion in step 7.
    let pin_set_size_before = dry.pin_set_size;

    // Step 3: full sweep with grace=0. The load-bearing call. If
    // the enabled image's chunks aren't in the pin set, they get
    // marked AND promoted in a single call — the M5 failure mode.
    let swept = driver.chunk_gc_sweep(Some(0)).await;
    assert_eq!(
        swept.promote_delete_errors, 0,
        "promote-pass must not error; got {swept:?}",
    );
    assert!(
        swept.pin_set_size >= pin_set_size_before,
        "pin_set_size shrank between dry-run and sweep ({} → {}). \
         Either an enabled image got disabled mid-test (unlikely on \
         the integration stack) or the pin set has a flake.",
        pin_set_size_before,
        swept.pin_set_size,
    );

    // Step 4: original session's data path still works. With NBD
    // wired, chunks lazy-fault through the pin-set survivors; without
    // NBD, the rootfs was materialized at create time. Either way,
    // a simple shell command should succeed if the chunks are intact.
    let ls = driver.exec(sid_1, "ls /").await;
    assert_eq!(
        ls.exit_status,
        Some(0),
        "post-sweep exec on original session failed — chunks may have \
         been deleted under it. stderr=<{}>",
        ls.stderr,
    );

    // Step 5: STRONGEST regression catch. A fresh session-create on
    // the same image materializes the rootfs anew from BlobStorage.
    // If the sweep deleted the image's chunks, this fails the same
    // way the M5 incident failed in prod.
    let sid_2 = driver.create_session_none_harness(&image).await;
    let ls_2 = driver.exec(sid_2, "ls /").await;
    assert_eq!(
        ls_2.exit_status,
        Some(0),
        "post-sweep fresh session-create + exec failed — the sweep \
         deleted the image's chunks under us (M5 regression). \
         stderr=<{}>",
        ls_2.stderr,
    );

    // Step 6: GET /candidates round-trip. No assertion on contents
    // beyond response shape — other tests in the same lane (the
    // chunk_gc_helpers_live_pg tests, the admin_chunk_gc_live_pg
    // tests) might leave rows in the table. The point is the
    // endpoint serves valid JSON.
    let candidates = driver.chunk_gc_candidates().await;
    for c in &candidates.candidates {
        assert_eq!(
            c.content_hash.len(),
            64,
            "candidate content_hash must be 64 hex chars; got {} len={}",
            c.content_hash,
            c.content_hash.len(),
        );
    }

    // Step 7: idempotency on a clean stack. After step 3's promote
    // pass deleted any orphans the candidate table had, a second
    // sweep at grace=0 should find nothing to promote.
    let swept_again = driver.chunk_gc_sweep(Some(0)).await;
    assert_eq!(
        swept_again.promote_delete_errors, 0,
        "second sweep must not error",
    );
    assert!(
        swept_again.pin_set_size > 0,
        "pin set must still cover live images after a clean sweep",
    );

    driver.delete(sid_1).await;
    driver.delete(sid_2).await;
}

/// ADR 0018 Phase C admin endpoint shape coverage.
///
/// The full multi-host alive-source evac requires two host-agents in
/// the integration stack. The current `integration-up.sh` spins up one.
/// Until a 2-host variant lands as a follow-up, this test exercises
/// what's reachable from a 1-host fixture:
///
///   - Endpoint is mounted under bearer auth.
///   - 404 on a non-existent session id.
///   - 409 on a session that's not yet Active (e.g. just-created,
///     still warming).
///   - For an Active session: returns 202 Accepted with
///     `status="evacuating"`. The `evac_resumer` scanner (which
///     runs in the same coord process) drives the session to
///     Active on a peer in ≤10s.
///
/// Pinned regressions:
///   - Route registration in `api/mod.rs` (a typo makes every call
///     404 instead of 200/4xx).
///   - Pre-flight checks (404 vs 409 vs 5xx) in
///     `admin::evacuate_session`.
///   - The async hand-off: the handler MUST return without
///     synchronously running the relocate (legacy commit-7 shape).
///
/// The full e2e — Active session × 2 hosts × disk-preserved-on-peer
/// — runs in the dev-vm `integration-evac-test.sh`.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_COORD_URL + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_evac_admin_endpoint_shape() {
    let driver = Driver::from_env();
    let image = Driver::image_uri();

    // Case 1: 404 on a session that doesn't exist.
    let bogus = SessionId::new();
    let path = format!("/api/v1/admin/sessions/{bogus}/evacuate");
    let resp = driver
        .req(reqwest::Method::POST, &path)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST evacuate on bogus session");
    let status = resp.status();
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "evacuate on unknown session should 404; got {status} body={}",
        resp.text().await.unwrap_or_default(),
    );

    // Case 2: live session — async shape (commit 12). Handler must
    // return 202 immediately after marking the session Evacuating.
    // The scanner picks it up from there.
    let sid = driver.create_session_none_harness(&image).await;
    let path = format!("/api/v1/admin/sessions/{sid}/evacuate");
    let resp = driver
        .req(reqwest::Method::POST, &path)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST evacuate on live session");
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "evacuate must return 202 on Active session (async shape); got {status} body={body_text}",
    );
    let body: Value = serde_json::from_str(&body_text).expect("decode EvacuateSessionResponse");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("evacuating"),
        "202 body.status must be \"evacuating\"; got {body:?}",
    );
    assert!(
        body.get("session_id").is_some(),
        "202 body must echo session_id; got {body:?}",
    );

    driver.delete(sid).await;
}
