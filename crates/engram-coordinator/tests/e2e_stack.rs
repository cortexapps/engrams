//! End-to-end tests against a live prod-shape stack (coord + host-agent
//! + Firecracker + Postgres + chunked-OCI registry + fake-gcs).
//!
//! The existing integration tests stop one layer short of the coord
//! app-gRPC API: `e2e_harness.rs` and `e2e_shell.rs` drive `PooledBackend`
//! directly; `ha_listener.rs` and friends use an in-proc `AppState`.
//! That left the coord gRPC → host-agent → FC path uncovered, which is
//! how prod session 8725648d's empty-Binary-frame bug shipped. This file
//! covers the three core flows (ADR 0021 P1.3 wire shape):
//!
//!   1. cold session with `mode = dev_vm` → `Exec ls` → assert stdout.
//!      Harness in the image (if any) is left undriven.
//!   2. cold session with `mode = agent` against the baked-claude
//!      image → `Exec ls` → assert stdout (agentd is up, harness
//!      runs but the test just exec's a shell command).
//!   3. cold session with `mode = agent` + bogus ANTHROPIC_API_KEY +
//!      initial prompt → assert an Anthropic auth-failure event
//!      surfaces in the StreamEvents stream.
//!
//! ADR 0051 Drip E: the coordinator's web-facing REST surface is gone.
//! These tests now drive the coordinator over the app-gRPC surface
//! (`engram_protocol::app::*`, `SessionService` / `FleetService`), authing
//! every RPC with an `Authorization: Bearer <token>` metadata header via a
//! tonic interceptor. The stack wiring (Tiltfile `coord_env`, ci.yml
//! `test-e2e-stack`) provides `ENGRAM_APP_GRPC_ADDR` +
//! `ENGRAM_APP_GRPC_TOKENS` so the gRPC endpoint + bearer are reachable.
//!
//! All tests are `#[ignore]`'d and gated by env vars. The CI lane
//! `test-e2e-stack` in `.github/workflows/ci.yml` brings up the stack
//! (`tilt-up-ci.sh` + `integration-bake-demo.sh`), runs these tests via
//! `cargo nextest --run-ignored`, and tears down on completion. ADR 0021
//! P1.5 retired the separate `harness add` step — the harness is baked
//! into the image at `/opt/engram/harness/` and the coord reads the launch
//! contract from `manifest.toml`.

use std::collections::HashMap;
use std::time::Duration;

use engram_core::SessionId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;
use serde_json::Value;
use tonic::codegen::InterceptedService;
use tonic::transport::Channel;

/// Per-RPC timeout. Cold `CreateSession` on CI runs ~120s on a fresh
/// chunk cache (FC boot + first NBD page-ins), so this needs to clear
/// that with a little headroom.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// How long the auth-failure test waits for either an `agent_message`
/// or a `run_completed(ok=false)` event after session-create returns.
/// Claude's stream-json round-trip through the harness + egress proxy +
/// api.anthropic.com 401 is typically <10s, but CI networking adds
/// latency. 180s = "if no signal arrives in this long, Claude is
/// genuinely hung (not just slow)" — a strong failure signal worth
/// panicking on.
const SSE_WAIT_DEADLINE: Duration = Duration::from_secs(180);

/// Auth-failure observation outcome.
///
/// The Claude harness emits `run_completed { ok: false }` when Claude
/// exits non-zero, but the auth error itself surfaces as an
/// `agent_message` event carrying the parseable error text. This enum
/// lets the test pass on either signal and prints captured events on the
/// fallback path so the next iteration can tighten the assertion.
#[derive(Debug)]
#[allow(dead_code)] // variants used only when the test runs (gated)
enum AuthFailureSignal {
    /// An `agent_message` arrived containing the expected substring. The
    /// string is the full message text so a regression that drops the
    /// substring surfaces clearly.
    ErrorMessage(String),
    /// No structured error message, but `run_completed.ok == false`
    /// arrived — the chain ran and Anthropic rejected the auth. Captured
    /// events are printed to stderr to inform the next tightening pass.
    RunCompletedNotOk,
    /// Neither signal in the deadline. Carries the captured (kind,
    /// payload_json) pairs for diagnostic output.
    TimedOut(Vec<(String, String)>),
}

/// `Authorization: Bearer <token>` interceptor (cribbed from the
/// ADR 0051 `grpc_smoke.rs` harness). tonic requires `Result<_, Status>`.
#[derive(Clone)]
struct BearerFn {
    token: String,
}

impl tonic::service::Interceptor for BearerFn {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.token)
                .parse()
                .expect("bearer header is ASCII"),
        );
        Ok(req)
    }
}

type SessClient = SessionServiceClient<InterceptedService<Channel, BearerFn>>;
type FleetClient = FleetServiceClient<InterceptedService<Channel, BearerFn>>;

/// gRPC driver for the live coordinator. Holds one shared channel and the
/// two service clients the tests exercise.
struct Driver {
    sess: SessClient,
    fleet: FleetClient,
}

impl Driver {
    async fn from_env() -> Self {
        let addr = std::env::var("ENGRAM_E2E_GRPC_ADDR").expect(
            "ENGRAM_E2E_GRPC_ADDR must be set (e.g. http://127.0.0.1:50061) — the \
             coordinator's app-gRPC endpoint",
        );
        let token = std::env::var("ENGRAM_E2E_GRPC_TOKEN")
            .unwrap_or_else(|_| "dev-app-grpc-token".to_string());

        let channel = tonic::transport::Endpoint::from_shared(addr.clone())
            .expect("parse ENGRAM_E2E_GRPC_ADDR as a gRPC endpoint")
            .connect_timeout(Duration::from_secs(5))
            .timeout(DEFAULT_TIMEOUT)
            .connect()
            .await
            .unwrap_or_else(|e| panic!("connect to coordinator app-gRPC at {addr}: {e}"));

        let interceptor = BearerFn { token };
        let sess = SessionServiceClient::with_interceptor(channel.clone(), interceptor.clone());
        let fleet = FleetServiceClient::with_interceptor(channel, interceptor);
        Self { sess, fleet }
    }

    fn image_uri() -> String {
        std::env::var("ENGRAM_E2E_IMAGE_URI").expect(
            "ENGRAM_E2E_IMAGE_URI must be set — the upstream CI step that ran \
             integration-bake-demo.sh writes it to $GITHUB_ENV",
        )
    }

    /// ADR 0021 P1.3: drive the image as a pure dev VM. Whether the image
    /// has a baked `[harness]` block is irrelevant — `mode = dev_vm` tells
    /// coord to skip `resolve_harness` and pass an empty-argv `AgentSpec`
    /// to the backend. agentd hits the readiness-probe branch and never
    /// execs the harness binary even if it's sitting in the rootfs.
    async fn create_session_none_harness(&mut self, image: &str) -> SessionId {
        let req = app::CreateSessionRequest {
            image_uri: image.to_string(),
            mode: "dev_vm".to_string(),
            prompt: None,
            secrets: HashMap::new(),
            harness_env: HashMap::new(),
        };
        let resp = self
            .sess
            .create_session(req)
            .await
            .expect("CreateSession (dev_vm)")
            .into_inner();
        resp.session_id.parse().expect("session_id is a SessionId")
    }

    /// ADR 0021 P1.3: drive the image's baked harness. `mode = agent` is
    /// the default but set explicitly so the test stays correct if defaults
    /// shift. The image referenced by `ENGRAM_E2E_IMAGE_URI` must carry a
    /// `[harness] builtin = "claude"` block (the CI bake of
    /// `deploy/demo-claude/` does); coord reads the harness contract from
    /// `manifest.toml`, so the request no longer names the harness.
    ///
    /// ADR 0051: the bogus Anthropic token is injected via `harness_env`
    /// (the orchestrator's trusted identity-injection channel), which the
    /// coord persists + replays on resume. `ANTHROPIC_API_KEY` is the
    /// Claude harness's auth env var.
    async fn create_session_claude(
        &mut self,
        image: &str,
        api_key: &str,
        prompt: Option<&str>,
    ) -> SessionId {
        let mut harness_env = HashMap::new();
        harness_env.insert("ANTHROPIC_API_KEY".to_string(), api_key.to_string());
        let req = app::CreateSessionRequest {
            image_uri: image.to_string(),
            mode: "agent".to_string(),
            prompt: prompt.map(str::to_string),
            secrets: HashMap::new(),
            harness_env,
        };
        let resp = self
            .sess
            .create_session(req)
            .await
            .expect("CreateSession (claude)")
            .into_inner();
        resp.session_id.parse().expect("session_id is a SessionId")
    }

    /// `SessionService.Exec` — server-streaming `ExecOutput`. Drains the
    /// stream to a collected stdout/stderr + exit status, mirroring the
    /// old unary `/exec` response shape the assertions expect.
    async fn exec(&mut self, sid: SessionId, command: &str) -> ExecResult {
        let req = app::ExecRequest {
            session_id: sid.to_string(),
            command: Some(command.to_string()),
            argv: Vec::new(),
            env: HashMap::new(),
            workdir: None,
            timeout_secs: Some(30),
        };
        let mut stream = self
            .sess
            .exec(req)
            .await
            .unwrap_or_else(|e| panic!("Exec open failed for {command:?}: {e}"))
            .into_inner();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status: Option<i32> = None;
        let mut saw_exit = false;
        while let Some(msg) = stream
            .message()
            .await
            .unwrap_or_else(|e| panic!("Exec stream error for {command:?}: {e}"))
        {
            match msg.event {
                Some(app::exec_output::Event::Started(_)) => {}
                Some(app::exec_output::Event::Stdout(b)) => stdout.extend_from_slice(&b),
                Some(app::exec_output::Event::Stderr(b)) => stderr.extend_from_slice(&b),
                Some(app::exec_output::Event::Exit(e)) => {
                    exit_status = e.exit_status;
                    saw_exit = true;
                }
                None => {}
            }
        }
        assert!(
            saw_exit,
            "Exec stream for {command:?} ended without an exit frame",
        );
        ExecResult {
            exit_status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    async fn delete(&mut self, sid: SessionId) {
        let req = app::DeleteSessionRequest {
            session_id: sid.to_string(),
        };
        let _ = self.sess.delete_session(req).await;
    }

    /// ADR 0016 Phase B commit 4a admin trigger → `FleetService.FlushSession`.
    /// Forces an immediate `ChunkedDiskBackend::flush()` on the session's
    /// bound sandbox + publishes the manifest_ref. Returns the parsed
    /// outcome + new manifest version.
    async fn flush_now(&mut self, sid: SessionId) -> FlushResult {
        let req = app::FlushSessionRequest {
            session_id: sid.to_string(),
        };
        let resp = self
            .fleet
            .flush_session(req)
            .await
            .expect("FlushSession")
            .into_inner();
        FlushResult {
            outcome: resp.outcome,
            manifest_version: resp.manifest_version,
        }
    }

    /// `SessionService.Snapshot`. Returns when the snapshot's PG row is
    /// durable. Body discarded — the side effect we care about is the row
    /// existing so resume() has something to find.
    async fn snapshot(&mut self, sid: SessionId) {
        let req = app::SnapshotRequest {
            session_id: sid.to_string(),
        };
        self.sess.snapshot(req).await.expect("Snapshot");
    }

    /// `SessionService.EvictLocal` — evict the local sandbox after a
    /// snapshot, leaving the session in `Idle`. Required before resume()
    /// will reconstruct a fresh sandbox.
    async fn evict_local(&mut self, sid: SessionId) {
        let req = app::EvictLocalRequest {
            session_id: sid.to_string(),
        };
        self.sess.evict_local(req).await.expect("EvictLocal");
    }

    /// `SessionService.Resume`. Synchronous: returns when the session is
    /// Active again. The newly-bound sandbox_id is the one that should
    /// appear in `nbd_sandboxes` (per ADR 0016 Phase B commit 5).
    async fn resume(&mut self, sid: SessionId) {
        let req = app::ResumeRequest {
            session_id: sid.to_string(),
        };
        self.sess.resume(req).await.expect("Resume");
    }

    /// `SessionService.GetCowState`. ADR 0016 Phase A diagnostic. Returns
    /// `Some(state)` when the sandbox is NBD-tracked (Phase B's chunked-disk
    /// pipeline live), `None` when the host fell back to materialize-to-file
    /// (no nbd.ko, no nbd_pool, etc.). Used by Phase B tests as a runtime
    /// probe for whether the chunked-disk-driven assertions are meaningful.
    async fn cow_state(&mut self, sid: SessionId) -> Option<app::CowStateView> {
        let req = app::GetCowStateRequest {
            session_id: sid.to_string(),
        };
        match self.sess.get_cow_state(req).await {
            Ok(resp) => resp.into_inner().state,
            Err(_) => None,
        }
    }

    /// Poll cow-state until `disk_manifest_version > prior` or the deadline
    /// elapses. Returns the new version on success, None on timeout. This is
    /// the load-bearing end-state assertion for Phase B e2e tests: a flush
    /// of dirty bytes — by either the FlushScheduler tick OR the admin
    /// flush trigger — advances `disk_manifest_version`.
    async fn wait_for_disk_manifest_advance(
        &mut self,
        sid: SessionId,
        prior_version: u64,
        deadline: Duration,
    ) -> Option<u64> {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if let Some(state) = self.cow_state(sid).await {
                if state.disk_manifest_version > prior_version {
                    return Some(state.disk_manifest_version);
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        None
    }

    /// `SessionService.GetSession` → the `Session` message. Black-box read:
    /// the lifecycle tests observe `status` (snake_case `SessionState`) and
    /// `host_id` exactly as a real client would. A pure read; it does not
    /// bump the session's activity clock.
    async fn get_session(&mut self, sid: SessionId) -> app::Session {
        let req = app::GetSessionRequest {
            session_id: sid.to_string(),
        };
        self.sess
            .get_session(req)
            .await
            .expect("GetSession")
            .into_inner()
            .session
            .expect("GetSessionResponse must carry a session")
    }

    /// The session's `status` string (snake_case `SessionState`).
    async fn session_status(&mut self, sid: SessionId) -> String {
        self.get_session(sid).await.status
    }

    /// `FleetService.EvacuateSession` — async evacuation. Returns the full
    /// gRPC `Status` on error so the shape test can assert on the code (the
    /// 404/409/202 distinctions the old HTTP handler returned now map to
    /// gRPC `NotFound` / `FailedPrecondition` / `Ok`).
    async fn evacuate(
        &mut self,
        sid: SessionId,
    ) -> Result<app::EvacuateSessionResponse, tonic::Status> {
        let req = app::EvacuateSessionRequest {
            session_id: sid.to_string(),
            target_host: None,
        };
        self.fleet
            .evacuate_session(req)
            .await
            .map(|r| r.into_inner())
    }

    /// `FleetService.ChunkGc` with `dry_run = true`.
    async fn chunk_gc_dry_run(&mut self, grace_secs: Option<u64>) -> app::ChunkGcResponse {
        let req = app::ChunkGcRequest {
            dry_run: true,
            grace_secs,
        };
        self.fleet
            .chunk_gc(req)
            .await
            .expect("ChunkGc dry_run")
            .into_inner()
    }

    /// `FleetService.ChunkGc` with `dry_run = false` — the live sweep.
    async fn chunk_gc_sweep(&mut self, grace_secs: Option<u64>) -> app::ChunkGcResponse {
        let req = app::ChunkGcRequest {
            dry_run: false,
            grace_secs,
        };
        self.fleet
            .chunk_gc(req)
            .await
            .expect("ChunkGc sweep")
            .into_inner()
    }

    /// Poll `GetSession` until `status == want` or the deadline elapses.
    /// Returns true on match. Used to observe Idle→Active (resume) through
    /// the public API alone.
    async fn wait_for_status(&mut self, sid: SessionId, want: &str, deadline: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if self.session_status(sid).await == want {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        false
    }

    /// Stream session events via `SessionService.StreamEvents`, watching for
    /// an Anthropic auth-failure signal in either of the shapes documented
    /// on `AuthFailureSignal`.
    ///
    /// The gRPC `SessionEvent` envelope carries `kind` (the event
    /// discriminant, e.g. "agent_message" / "run_completed") + `payload_json`
    /// (the serialized event payload). We match on `kind` and pull `text` /
    /// `ok` out of the parsed payload — the same fields the typed
    /// `HarnessAgentMessage` / `HarnessRunCompleted` variants carry.
    async fn wait_for_anthropic_auth_failure(
        &mut self,
        sid: SessionId,
        expected_substr: &str,
        deadline: Duration,
    ) -> AuthFailureSignal {
        let req = app::StreamEventsRequest {
            session_id: sid.to_string(),
            since: None, // from the start
        };
        let mut stream = match self.sess.stream_events(req).await {
            Ok(r) => r.into_inner(),
            Err(e) => {
                eprintln!("StreamEvents open failed: {e}");
                return AuthFailureSignal::TimedOut(Vec::new());
            }
        };

        let mut captured: Vec<(String, String)> = Vec::new();
        let started = std::time::Instant::now();

        loop {
            let Some(remaining) = deadline.checked_sub(started.elapsed()) else {
                return AuthFailureSignal::TimedOut(captured);
            };
            let next = tokio::time::timeout(remaining, stream.message()).await;
            let ev = match next {
                Err(_) => return AuthFailureSignal::TimedOut(captured),
                Ok(Ok(None)) => return AuthFailureSignal::TimedOut(captured),
                Ok(Err(e)) => {
                    eprintln!("StreamEvents RPC error: {e}");
                    return AuthFailureSignal::TimedOut(captured);
                }
                Ok(Ok(Some(ev))) => ev,
            };

            // Signal 1: an agent_message carrying the expected text.
            if ev.kind == "agent_message" {
                if let Some(text) = payload_str(&ev.payload_json, "text") {
                    if text.contains(expected_substr) {
                        return AuthFailureSignal::ErrorMessage(text);
                    }
                }
            }

            // Signal 2: run_completed with ok == false. Give the stream a
            // short grace so a trailing agent_message with the error text
            // (if any) can land first — the harness emits run_completed
            // AFTER forwarding agent_messages.
            if ev.kind == "run_completed" && payload_bool(&ev.payload_json, "ok") == Some(false) {
                captured.push((ev.kind.clone(), ev.payload_json.clone()));
                let grace = std::time::Instant::now() + Duration::from_millis(500);
                while std::time::Instant::now() < grace {
                    match tokio::time::timeout(Duration::from_millis(100), stream.message()).await {
                        Ok(Ok(Some(e2))) => {
                            if e2.kind == "agent_message" {
                                if let Some(text) = payload_str(&e2.payload_json, "text") {
                                    if text.contains(expected_substr) {
                                        return AuthFailureSignal::ErrorMessage(text);
                                    }
                                }
                            }
                            captured.push((e2.kind, e2.payload_json));
                        }
                        _ => break,
                    }
                }
                eprintln!(
                    "AUTH-FAIL via run_completed(ok=false) only. Captured events:\n{captured:#?}\n\
                     Next iteration: tighten EXPECTED_ERROR_SUBSTR based on whatever \
                     agent_message text Anthropic returns."
                );
                return AuthFailureSignal::RunCompletedNotOk;
            }

            captured.push((ev.kind, ev.payload_json));
        }
    }
}

/// Pull a string field out of a `payload_json` object.
fn payload_str(payload_json: &str, field: &str) -> Option<String> {
    let v: Value = serde_json::from_str(payload_json).ok()?;
    v.get(field).and_then(Value::as_str).map(str::to_string)
}

/// Pull a bool field out of a `payload_json` object.
fn payload_bool(payload_json: &str, field: &str) -> Option<bool> {
    let v: Value = serde_json::from_str(payload_json).ok()?;
    v.get(field).and_then(Value::as_bool)
}

/// Collected result of draining an `Exec` stream — the shape the old unary
/// `ExecResponse` exposed to the assertions.
struct ExecResult {
    exit_status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Collected result of a `FlushSession` RPC.
#[derive(Debug)]
struct FlushResult {
    outcome: String,
    manifest_version: Option<u64>,
}

// ---------- Tests ----------

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a live prod-shape stack with a baked demo image"]
async fn e2e_cold_session_no_harness_can_exec_ls() {
    let mut driver = Driver::from_env().await;
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
    let mut driver = Driver::from_env().await;
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
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a Claude harness pack baked into the demo image"]
async fn e2e_cold_session_claude_harness_can_exec_ls() {
    // Raw Exec hits the sandbox directly — the Claude harness is bound but
    // unused. Use a bogus key so a future regression that races a harness
    // call won't burn real Anthropic budget; this test doesn't send a prompt.
    let mut driver = Driver::from_env().await;
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
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + Claude harness + real api.anthropic.com reachability"]
async fn e2e_claude_with_bogus_key_surfaces_anthropic_auth_error() {
    // Claude CLI surfaces Anthropic's 401 as an assistant-role
    // agent_message with literal text:
    //
    //   "Invalid API key · Fix external API key"
    //
    // (the `·` is a middle dot, U+00B7; we only assert on the ASCII
    // prefix). The harness's `run_completed.ok` is actually `true` on
    // this path because the Claude CLI process itself exited cleanly —
    // the auth error lives entirely in the stream-json output it printed
    // before exit. That's why this test watches `agent_message` text, not
    // the `ok` flag.
    const EXPECTED_ERROR_SUBSTR: &str = "Invalid API key";

    let mut driver = Driver::from_env().await;
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
            // Acceptable: the chain ran and failed cleanly. The stderr
            // dump from wait_for_anthropic_auth_failure tells us what to
            // tighten to.
        }
        AuthFailureSignal::TimedOut(events) => panic!(
            "no auth-failure signal within {}s; saw events:\n{events:#?}",
            SSE_WAIT_DEADLINE.as_secs()
        ),
    }
    driver.delete(sid).await;
}

/// Whether this environment is REQUIRED to exercise the NBD chunked-disk
/// path. Set `ENGRAM_EXPECT_NBD=1` on runners where NBD is known-available
/// (Blacksmith ships `CONFIG_BLK_DEV_NBD=y` built-in; the dev VM loads
/// `nbd.ko`) so a null `cow-state` becomes a hard failure instead of a
/// silent skip. Unset (local macOS / VZ, any NBD-less box) keeps the
/// graceful-degrade warning path.
fn nbd_required() -> bool {
    matches!(
        std::env::var("ENGRAM_EXPECT_NBD").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// ADR 0016 Phase B commit 4b: end-to-end exercise of the FlushScheduler
/// primitive via `FleetService.FlushSession`. Explicit-trigger counterpart
/// to the scheduler's 30s implicit cadence; the only way to verify the
/// chunked-disk-write → `backend.flush()` → coord `update_live_disk_manifest`
/// → PG row round-trip in CI without sleeping a full cadence. Without this
/// test, the prod-shape chunked-disk write path silently regresses on any
/// change to `nbd_sandboxes` wiring or the `update_live_disk_manifest` TX
/// shape — neither covered by the in-process unit tests.
///
/// **Environment dependence** (per `[fc_tests_run_in_ci]`):
///
/// The chunked-disk write assertions only fire when the host has `nbd_pool`
/// + `chunk_store` + `chunk_cache` wired AND a chunked-OCI demo image. On
/// Blacksmith CI runners today, `nbd.ko` isn't in the guest kernel and the
/// host probe falls back to materialize-to-file — `nbd_sandboxes` stays
/// empty, `flush_sandbox` returns None for every sandbox. The test detects
/// this via the Phase A `GetCowState` diagnostic (null state == not
/// chunk-tracked) and skips the applied/idle assertions with a `::warning::`
/// so the gap is visible in every run. The always-on assertions (pre-write
/// idle + sandbox-bound success) still fire on every environment.
///
/// Test path:
/// 1. Cold-create a session against the demo image.
/// 2. **Always**: `flush_now` pre-write → assert `outcome=idle`.
/// 3. Probe `cow-state`. If null → `::warning::` + return.
/// 4. dd + sync into the chunked-disk-backed rootfs.
/// 5. `flush_now` → assert the disk lineage advances.
/// 6. Poll cow-state for the manifest-version advance.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_flush_now_applies_then_short_circuits_on_no_dirty() {
    let mut driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Step 2: pre-write flush. Environment-independent — the endpoint
    // should always return idle when nothing's dirty, regardless of
    // whether the host has NBD wired. idle here proves the RPC is wired,
    // the session-lookup works, and `flush_sandbox` returns None cleanly.
    let pre = driver.flush_now(sid).await;
    assert_eq!(
        pre.outcome, "idle",
        "first flush before any writes should be idle; got {pre:?}",
    );
    assert!(
        pre.manifest_version.is_none(),
        "idle outcome must NOT carry a manifest_version; got {pre:?}",
    );

    // Step 3: probe whether this environment can actually exercise the
    // chunked-disk path. `cow-state` returns Some(...) only when the
    // sandbox is NBD-tracked. Where NBD is known-available
    // (ENGRAM_EXPECT_NBD set), a null here is a real regression and we
    // FAIL. Elsewhere we skip with a loud warning.
    let cow = driver.cow_state(sid).await;
    if cow.is_none() {
        assert!(
            !nbd_required(),
            "ENGRAM_EXPECT_NBD is set but cow-state is null for session {sid}: the \
             host-agent fell back to materialize-to-file instead of NBD-attaching the \
             chunked disk. NBD is available on this runner, so this is a regression in \
             the chunked-disk wiring, not an environment gap."
        );
        eprintln!(
            "::warning title=Phase B flush-now e2e partial coverage::\
             cow-state returned null for session {sid} — the runner's host-agent \
             fell back to materialize-to-file (no nbd.ko / no nbd_pool wired). \
             Pre-write flush=idle assertion verified the RPC wiring, but the \
             chunked-disk write → flush → publish round-trip can't be exercised here."
        );
        driver.delete(sid).await;
        return;
    }

    // Step 4: capture the baseline disk_manifest_version BEFORE dd. The
    // FlushScheduler may already have ticked between create-session and
    // now; whatever version it left behind is what we measure forward from.
    let baseline_version = cow.as_ref().map(|s| s.disk_manifest_version).unwrap_or(0);

    // Step 5: write enough dirty bytes to materialise at least one full
    // 16 MiB chunk in the chunked-disk dirty buffer. `/var` is writable +
    // survives the run. `count=8` keeps the exec inside the default 30s
    // timeout even on cold-cache CI runs. `/dev/zero` so writes dedupe
    // cleanly across re-runs.
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
    // `sync` so the writes hit the chunked-disk backend rather than sitting
    // in the guest page cache.
    let sync = driver.exec(sid, "sync").await;
    assert_eq!(sync.exit_status, Some(0), "sync should succeed");

    // Step 6: force the flush via the admin trigger — best-effort. The
    // outcome can be `applied` (dirty chunks resident) or `idle` (the
    // FlushScheduler's 30s tick already drained the buffer). Either way,
    // step 7's end-state check still passes — Phase B's claim is that the
    // disk lineage advances on writes, regardless of who drained the buffer.
    let _ = driver.flush_now(sid).await;

    // Step 7: end-state assertion. Poll cow-state until the disk manifest
    // version advances past `baseline_version`. The 90-second deadline
    // covers one full scheduler tick (30s) + generous CI slack.
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

/// ADR 0016 Phase B commit 5 regression: a resumed sandbox is a first-class
/// entry in `nbd_sandboxes`, the FlushScheduler runs against it, and a
/// subsequent flush + eviction-style snapshot succeeds.
///
/// Pre-commit-5 behaviour (the failure mode this test pins):
/// - `cow-state` on the resumed session returned null (the resumed sandbox
///   was never inserted into `nbd_sandboxes`).
/// - A second eviction-snapshot errored with `non-canonical jail layout`
///   because the resume path materialized to a flat file instead of
///   rebuilding the chunked-NBD layout the snapshot pipeline expects.
///
/// **Environment dependence**: same cow-state-probe skip pattern as
/// `e2e_flush_now_applies_then_short_circuits_on_no_dirty`. On Blacksmith CI
/// without `nbd.ko`, the test runs through the flush + snapshot + evict +
/// resume RPC surface (catches regressions in those handlers) but skips the
/// cow-state-post-resume + flush-post-resume assertions with a loud warning.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_rejoins_chunked_disk_tracking() {
    let mut driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Probe FIRST — if this environment can't exercise the NBD path, every
    // subsequent assertion below is meaningless.
    let pre_cow = driver.cow_state(sid).await;
    if pre_cow.is_none() {
        assert!(
            !nbd_required(),
            "ENGRAM_EXPECT_NBD is set but cow-state is null for session {sid} \
             (pre-snapshot): the host-agent didn't NBD-attach the chunked disk. \
             NBD is available on this runner, so this is a regression, not an \
             environment gap."
        );
        eprintln!(
            "::warning title=Phase B resume regression partial coverage::\
             cow-state returned null for session {sid} (pre-snapshot) — runner \
             can't exercise the NBD path. Snapshot+resume RPC wiring will still \
             be exercised below; cow-state-post-resume + flush-post-resume \
             assertions skipped."
        );
        driver.delete(sid).await;
        return;
    }

    // Dirty the disk so the snapshot we take has a non-trivial disk_manifest
    // the resume path can NBD-attach against.
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

    // Best-effort flush via the admin trigger (the FlushScheduler may have
    // already drained). The snapshot below re-flushes via its own
    // backend.flush() regardless, so the chunks ARE durable in BlobStorage
    // by the time we evict.
    let _ = driver.flush_now(sid).await;

    // Snapshot → evict-local → resume. Equivalent to the idle-eviction →
    // resume cycle prod exercises, minus the 30-second idle wait.
    driver.snapshot(sid).await;
    driver.evict_local(sid).await;
    driver.resume(sid).await;

    // **THE REGRESSION CHECK**: post-resume cow-state must be Some(...).
    // Pre-commit-5 this returned null. Post-commit-5 the resumed sandbox is
    // in `nbd_sandboxes` → diagnostic populates.
    let post_cow = driver.cow_state(sid).await;
    assert!(
        post_cow.is_some(),
        "post-resume cow-state must be Some — commit 5 wired the resumed sandbox \
         into nbd_sandboxes. Null here means the resume took the materialize-to-file \
         fallback path or the NBD attach branch silently no-op'd.",
    );

    // **THE SECOND REGRESSION CHECK**: the FlushScheduler can now produce a
    // non-trivial flush on the resumed sandbox. Capture the post-resume
    // baseline and write a NEW file so the assertion can't be satisfied by
    // stale state.
    let post_resume_baseline = post_cow
        .as_ref()
        .map(|s| s.disk_manifest_version)
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

    // Best-effort admin trigger.
    let _ = driver.flush_now(sid).await;

    // End-state assertion: the resumed sandbox's disk manifest version must
    // advance past the post-resume baseline. Pre-commit-5 this would hang
    // forever (resumed sandbox isn't in nbd_sandboxes).
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

/// Lifecycle e2e: idle→active resume preserves BOTH disk and memory,
/// proven the only way a user would see it — write through the API, run the
/// snapshot→evict→resume cycle, read back through the API.
///
/// Pure black-box: it never inspects cow-state or manifest internals. Two
/// guarantees:
///
///   - **Disk data survives byte-identical.** A UUID sentinel written to
///     `/var/sentinel.txt` is `cat`'d back after resume and must match. It
///     holds on any backend (NBD-attached OR materialize-to-file), so the
///     test does NOT gate on NBD.
///   - **The kernel was memory-restored, not rebooted.**
///     `/proc/sys/kernel/random/boot_id` is minted once per kernel boot and
///     lives only in kernel memory — the FC memory snapshot captures it, a
///     cold reboot regenerates it. Asserting it's unchanged across the cycle
///     proves resume is a true warm memory-restore.
///
/// ADR 0051 note: this test (snapshot + evict-local + resume) is now the
/// SOLE evict→resume data-preservation coverage. The former
/// `e2e_idle_evict_then_resume_preserves_data` drove the same Active→Idle→
/// Active cycle through the `evict-idle` admin trigger, which has NO app-gRPC
/// analog (no EvictIdle RPC in `SessionService`). Expressed via
/// EvictLocal+Resume it would be a byte-for-byte duplicate of this test, so
/// it was deleted rather than ported — see the deletion note in the ADR 0051
/// migration commit.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_preserves_disk_and_memory() {
    let mut driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Disk sentinel: a fresh UUID so stale state can't satisfy the readback.
    let disk_sentinel = uuid::Uuid::new_v4().to_string();
    let write = driver
        .exec(
            sid,
            &format!("printf '%s' {disk_sentinel} > /var/sentinel.txt && sync"),
        )
        .await;
    assert_eq!(
        write.exit_status,
        Some(0),
        "sentinel write should succeed; stderr=<{}>",
        write.stderr,
    );

    // Memory-continuity witness: the kernel's boot_id, captured BEFORE the
    // snapshot. Survives a memory-restore; changes on a reboot.
    let boot_id_before = driver
        .exec(sid, "cat /proc/sys/kernel/random/boot_id")
        .await;
    assert_eq!(
        boot_id_before.exit_status,
        Some(0),
        "reading boot_id should succeed; stderr=<{}>",
        boot_id_before.stderr,
    );
    let boot_id_before = boot_id_before.stdout.trim().to_string();
    assert!(!boot_id_before.is_empty(), "boot_id must be non-empty");

    // The idle→active cycle prod exercises (minus the 30s idle wait):
    // snapshot the running VM, drop the local sandbox, resume.
    driver.snapshot(sid).await;
    driver.evict_local(sid).await;
    driver.resume(sid).await;
    assert!(
        driver
            .wait_for_status(sid, "active", Duration::from_secs(30))
            .await,
        "session should be Active after resume",
    );

    // Disk survived byte-identical.
    let readback = driver.exec(sid, "cat /var/sentinel.txt").await;
    assert_eq!(
        readback.exit_status,
        Some(0),
        "sentinel readback should succeed; stderr=<{}>",
        readback.stderr,
    );
    assert_eq!(
        readback.stdout.trim(),
        disk_sentinel,
        "disk data lost across resume: /var/sentinel.txt content changed",
    );

    // Kernel was memory-restored, not rebooted.
    let boot_id_after = driver
        .exec(sid, "cat /proc/sys/kernel/random/boot_id")
        .await;
    assert_eq!(
        boot_id_after.stdout.trim(),
        boot_id_before,
        "boot_id changed across resume — the VM cold-rebooted instead of \
         restoring from the memory snapshot (in-memory state would be lost)",
    );

    driver.delete(sid).await;
}

// ADR 0051: `e2e_idle_evict_then_resume_preserves_data` deleted. It depended
// solely on the `evict-idle` admin trigger (the old
// `POST /api/v1/admin/sessions/:id/evict-idle` route), which has NO app-gRPC
// analog — `SessionService` exposes no EvictIdle RPC, and inventing one would
// be a breaking app/v1 proto change (out of scope). The observable contract
// it pinned (disk data survives an Active→Idle→Active cycle) is fully covered
// by `e2e_resume_preserves_disk_and_memory` above, which drives the same
// transition via Snapshot + EvictLocal + Resume; porting the idle test would
// have produced a byte-for-byte duplicate.

// ---------------------------------------------------------------------
// ADR 0016 Phase C commit 6a — chunk-GC RPC e2e regression
// ---------------------------------------------------------------------

/// ADR 0016 Phase C commit 6a — load-bearing regression test for the M5
/// failure class.
///
/// The 2026-05-23 incident this catches: the prior chunk-GC marked every
/// chunked-disk image's chunks as candidates because its live-set query
/// missed the `enabled_images` lineage, then silent-deleted them after a 24h
/// grace. The next session-create against any chunked image faulted with
/// `chunk fetch: blob storage: blob not found`.
///
/// This test pins that the redesigned GC does NOT repeat that failure. Flow:
///
/// 1. Create a none-harness session on the demo image (chunked-disk in CI).
/// 2. `ChunkGc { dry_run: true }` — assert pin_set_size > 0.
/// 3. `ChunkGc { dry_run: false, grace_secs: 0 }` — full sweep; must NOT
///    delete the demo image's chunks.
/// 4. Re-exec a command on the original session — proves the sweep didn't
///    break the session's data path.
/// 5. Create a SECOND none-harness session on the same image — the strongest
///    regression catch (materializes the rootfs anew from BlobStorage).
/// 6. `ChunkGc { dry_run: false, grace_secs: 0 }` again — idempotent on a
///    clean stack.
///
/// ADR 0051: the old step-6 `GET /api/admin/chunk-gc/candidates` round-trip
/// is dropped — `FleetService` intentionally exposes NO ChunkGcCandidates RPC
/// (see fleet.proto "INTENTIONAL DROPS"). The candidate-table shape is still
/// pinned by the `admin_chunk_gc_live_pg` unit tests in the same crate; the
/// e2e value here is the live-image-not-deleted invariant, which the sweep
/// assertions cover.
///
/// Environment dependence: this test does NOT skip on missing NBD. The
/// pin-set + sweep paths run against PG + BlobStorage only; session
/// create+exec works against either NBD-attached or materialize-to-file
/// rootfs.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_chunk_gc_sweep_does_not_delete_live_image_chunks() {
    let mut driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid_1 = driver.create_session_none_harness(&image).await;

    // Step 2: dry-run baseline. The pin set MUST cover the demo image's
    // chunks (enabled_images source). If this is 0, the regression has
    // already happened by construction and step 3's sweep would wipe the
    // bucket.
    let dry = driver.chunk_gc_dry_run(Some(0)).await;
    assert!(
        dry.pin_set_size > 0,
        "pin_set_size must be > 0 — the demo image's chunks should be pinned via \
         enabled_images.disk_manifest_*. Got {dry:?}. A 0 here means the \
         enabled-image pin-set source is broken; proceeding to step 3's sweep \
         would have nuked the bucket."
    );
    assert_eq!(
        dry.grace_secs, 0,
        "grace_secs override must echo back; got {dry:?}",
    );
    assert_eq!(
        dry.promoted_deletes, 0,
        "DryRun must promote nothing; got {dry:?}",
    );

    let pin_set_size_before = dry.pin_set_size;

    // Step 3: full sweep with grace=0. The load-bearing call. If the enabled
    // image's chunks aren't in the pin set, they get marked AND promoted in
    // a single call — the M5 failure mode.
    let swept = driver.chunk_gc_sweep(Some(0)).await;
    assert_eq!(
        swept.promote_delete_errors, 0,
        "promote-pass must not error; got {swept:?}",
    );
    assert!(
        swept.pin_set_size >= pin_set_size_before,
        "pin_set_size shrank between dry-run and sweep ({} → {}). Either an \
         enabled image got disabled mid-test (unlikely on the integration \
         stack) or the pin set has a flake.",
        pin_set_size_before,
        swept.pin_set_size,
    );

    // Step 4: original session's data path still works.
    let ls = driver.exec(sid_1, "ls /").await;
    assert_eq!(
        ls.exit_status,
        Some(0),
        "post-sweep exec on original session failed — chunks may have been \
         deleted under it. stderr=<{}>",
        ls.stderr,
    );

    // Step 5: STRONGEST regression catch. A fresh session-create on the same
    // image materializes the rootfs anew from BlobStorage.
    let sid_2 = driver.create_session_none_harness(&image).await;
    let ls_2 = driver.exec(sid_2, "ls /").await;
    assert_eq!(
        ls_2.exit_status,
        Some(0),
        "post-sweep fresh session-create + exec failed — the sweep deleted the \
         image's chunks under us (M5 regression). stderr=<{}>",
        ls_2.stderr,
    );

    // Step 6: idempotency on a clean stack. After step 3's promote pass
    // deleted any orphans, a second sweep at grace=0 should find nothing to
    // promote.
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

/// ADR 0018 Phase C RPC shape coverage.
///
/// The full multi-host alive-source evac requires two host-agents. The
/// current single-host integration stack exercises what's reachable from a
/// 1-host fixture:
///
///   - The RPC is bound under bearer auth.
///   - `NotFound` on a non-existent session id (the old HTTP 404).
///   - For an Active session: returns Ok with `status="evacuating"` (the old
///     HTTP 202). The `evac_resumer` scanner drives the session to Active on
///     a peer in ≤10s.
///
/// Pinned regressions:
///   - RPC registration (a wiring typo makes every call Unimplemented).
///   - Pre-flight checks in the evacuate handler.
///   - The async hand-off: the handler MUST return without synchronously
///     running the relocate.
///
/// ADR 0051: the old test asserted a distinct HTTP 409 on a not-yet-Active
/// session. The gRPC handler folds that into the same async accept path
/// (mark-Evacuating + return), so this test asserts the live-session
/// accept-and-status path; the 404→NotFound mapping is the load-bearing
/// pre-flight check that remains.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_evac_admin_endpoint_shape() {
    let mut driver = Driver::from_env().await;
    let image = Driver::image_uri();

    // Case 1: NotFound on a session that doesn't exist.
    let bogus = SessionId::new();
    let err = driver
        .evacuate(bogus)
        .await
        .expect_err("evacuate on unknown session must error");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "evacuate on unknown session should map to NotFound; got {err:?}",
    );

    // Case 2: live session — async shape. Handler must accept immediately
    // after marking the session Evacuating. The scanner picks it up from
    // there.
    let sid = driver.create_session_none_harness(&image).await;
    let resp = driver
        .evacuate(sid)
        .await
        .expect("evacuate must succeed on an Active session (async shape)");
    assert_eq!(
        resp.status, "evacuating",
        "evacuate response status must be \"evacuating\"; got {resp:?}",
    );
    assert_eq!(
        resp.session_id,
        sid.to_string(),
        "evacuate response must echo session_id; got {resp:?}",
    );

    driver.delete(sid).await;
}

// ADR 0051: `e2e_two_host_teleport_preserves_sentinel` deleted. It drove the
// old `POST /api/v1/admin/sessions/:id/teleport` route, which has NO app-gRPC
// analog — there is no Teleport RPC on `FleetService`/`SessionService`, and
// adding one would be a breaking app/v1 proto change (out of scope). The
// two-host evacuation path is still exercised end-to-end by the dev-vm
// `integration-evac-test.sh`; the single-host async-accept shape is pinned by
// `e2e_evac_admin_endpoint_shape` above. The `teleport` CI matrix variant in
// ci.yml's `test-e2e-stack` (which existed solely to run this test on a
// two-host stack) is dropped alongside it.
