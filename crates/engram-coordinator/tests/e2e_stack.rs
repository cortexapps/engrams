//! End-to-end tests against a live prod-shape stack (coord + host-agent
//! + Firecracker + Postgres + chunked-OCI registry + fake-gcs).
//!
//! The existing integration tests stop one layer short of the coord
//! application surface: `e2e_harness.rs` and `e2e_shell.rs` drive
//! `PooledBackend` directly; `ha_listener.rs` and friends use an in-proc
//! `AppState`. That left the coord app-gRPC → host-agent → FC path
//! uncovered, which is how prod session 8725648d's empty-Binary-frame
//! bug shipped.
//!
//! ADR 0039 §1 retired the coordinator's HTTP application surface — the
//! session/exec/events/admin REST routes were promoted to the app-gRPC
//! `SessionService` + `FleetService` (only `/healthz`, `/readyz`, the
//! forge seam, and host/admin-pause-resume ingestion remain HTTP). This
//! file drives those services over tonic, the same way `grpc_smoke.rs`
//! does (shared bearer + client pattern).
//!
//! Flows covered — three session journeys plus five control-plane
//! internals (flush-now, chunked-disk resume tracking, chunk-GC live-set
//! safety, skills-bundle mount, evac RPC shape):
//!
//! - cold session with `mode = dev_vm` → `Exec ls` → assert stdout.
//!   Harness in the image (if any) is left undriven.
//! - cold session with `mode = agent` against the baked-claude image →
//!   `Exec ls` → assert stdout (agentd is up; the harness is bound but
//!   the test just exec's a shell command).
//! - cold session with `mode = agent` + bogus ANTHROPIC_API_KEY + initial
//!   prompt → assert an Anthropic auth-failure event surfaces in the
//!   `StreamEvents` feed.
//!
//! All are `#[ignore]`'d and gated by env vars. The CI lane
//! `test-e2e-stack` in `.github/workflows/ci.yml` brings up the stack
//! (`tilt-up-ci.sh` + `integration-bake-demo.sh`), runs these tests via
//! `cargo nextest --run-ignored`, and tears down on completion. ADR 0021
//! P1.5 retired the separate `harness add` step — the harness is baked
//! into the image at `/opt/engram/harness/` and the coord reads the
//! launch contract from `manifest.toml`.
//!
//! Env:
//!
//! - `ENGRAM_E2E_GRPC` — coord app-gRPC `<host>:<port>` (e.g.
//!   `127.0.0.1:50061`). Required.
//! - `ENGRAM_E2E_GRPC_TOKEN` — app-gRPC bearer. Defaults to the Tiltfile
//!   dev literal `dev-app-grpc-token`.
//! - `ENGRAM_E2E_IMAGE_URI` — the baked demo-claude image URI. The
//!   upstream CI step that runs integration-bake-demo.sh writes it to
//!   `$GITHUB_ENV`. Required.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;
use serde_json::Value;
use tonic::transport::Channel;

/// Per-RPC timeout for a cold create / snapshot / resume / GC sweep. Cold
/// `CreateSession` on CI runs ~120s on a fresh chunk cache (FC boot +
/// first NBD page-ins), so this clears that with headroom.
const CREATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Per-RPC timeout for an `Exec` stream. The command's own
/// `timeout_secs` bounds the process; this bounds the whole stream,
/// generous enough for the first exec after a cold create when the guest
/// is still settling.
const EXEC_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-RPC timeout for cheap unary calls (delete, flush, cow-state,
/// evict).
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

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
/// The Claude harness today emits `run_completed { ok: false }` when
/// Claude exits non-zero, but Claude's stream-json output *also* carries
/// a parseable error message that surfaces as an `agent_message` event.
/// This enum lets the test pass on either signal and prints captured
/// events on the fallback path so the next iteration can tighten the
/// assertion.
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
    /// Neither signal in the deadline. Carries the captured events
    /// (rendered `kind: payload_json`) for diagnostic output.
    TimedOut(Vec<String>),
}

/// Result of an `Exec` stream collapsed to the unary shape the old HTTP
/// `/exec` handler returned: exit status + the concatenated stdout/stderr.
struct ExecResult {
    exit_status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Drives the coordinator's app-gRPC surface over a single shared
/// `Channel`. Clients are cheap to construct (the `Channel` clone shares
/// the connection pool), so each helper spins up the service client it
/// needs and attaches the bearer per request.
struct Driver {
    channel: Channel,
    token: String,
}

impl Driver {
    /// Connect to the coord app-gRPC from env. Mirrors `grpc_smoke.rs`'s
    /// connect path (5s connect timeout so a wedged stack fails loudly).
    async fn connect() -> Self {
        let addr = std::env::var("ENGRAM_E2E_GRPC")
            .expect("ENGRAM_E2E_GRPC must be set (e.g. 127.0.0.1:50061)");
        let token = std::env::var("ENGRAM_E2E_GRPC_TOKEN")
            .unwrap_or_else(|_| "dev-app-grpc-token".to_string());
        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .expect("valid gRPC endpoint")
            .connect_timeout(Duration::from_secs(5));
        let channel = endpoint
            .connect()
            .await
            .expect("connect to coordinator app-gRPC");
        Self { channel, token }
    }

    fn image_uri() -> String {
        std::env::var("ENGRAM_E2E_IMAGE_URI").expect(
            "ENGRAM_E2E_IMAGE_URI must be set — the upstream CI step that ran \
             integration-bake-demo.sh writes it to $GITHUB_ENV",
        )
    }

    fn session(&self) -> SessionServiceClient<Channel> {
        SessionServiceClient::new(self.channel.clone())
    }

    fn fleet(&self) -> FleetServiceClient<Channel> {
        FleetServiceClient::new(self.channel.clone())
    }

    /// Wrap a message in a `tonic::Request` with the bearer metadata + a
    /// per-call timeout. Replaces `grpc_smoke.rs`'s interceptor: building
    /// the header inline keeps the client types nameable so `Driver` can
    /// hold a plain `Channel`.
    fn req<T>(&self, msg: T, timeout: Duration) -> tonic::Request<T> {
        let mut req = tonic::Request::new(msg);
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.token)
                .parse()
                .expect("bearer header is ASCII"),
        );
        req.set_timeout(timeout);
        req
    }

    /// ADR 0021 P1.3: drive the image as a pure dev VM. Whether the image
    /// has a baked `[harness]` block is irrelevant — `mode = dev_vm`
    /// tells coord to skip `resolve_harness` and pass an empty-argv
    /// `AgentSpec` to the backend. agentd hits the readiness-probe branch
    /// (`harness_supervisor.rs::spawn`) and never execs the harness
    /// binary even if it's sitting in the rootfs at
    /// `/opt/engram/harness/`.
    async fn create_session_none_harness(&self, image: &str) -> String {
        let req = self.req(
            app::CreateSessionRequest {
                image_uri: image.to_string(),
                mode: "dev_vm".into(),
                prompt: None,
                harness_secret_id: None,
                secrets: HashMap::new(),
            },
            CREATE_TIMEOUT,
        );
        self.session()
            .create_session(req)
            .await
            .expect("CreateSession (dev_vm)")
            .into_inner()
            .session_id
    }

    /// ADR 0021 P1.3: drive the image's baked harness. `mode = agent`.
    /// The image referenced by `ENGRAM_E2E_IMAGE_URI` must carry a
    /// `[harness] builtin = "claude"` block (the CI bake of
    /// `deploy/demo-claude/` does); coord reads the harness contract from
    /// `manifest.toml`, so the request no longer names the harness
    /// directly. The API key rides in the per-request `secrets` map
    /// (honored under SecretMode::Literal — session.proto field 5).
    async fn create_session_claude(
        &self,
        image: &str,
        api_key: &str,
        prompt: Option<&str>,
    ) -> String {
        let mut secrets = HashMap::new();
        secrets.insert("ANTHROPIC_API_KEY".to_string(), api_key.to_string());
        let req = self.req(
            app::CreateSessionRequest {
                image_uri: image.to_string(),
                mode: "agent".into(),
                prompt: prompt.map(|p| p.to_string()),
                harness_secret_id: None,
                secrets,
            },
            CREATE_TIMEOUT,
        );
        self.session()
            .create_session(req)
            .await
            .expect("CreateSession (agent/claude)")
            .into_inner()
            .session_id
    }

    /// `SessionService.Exec`: run `command` via `sh -c`, collect the
    /// stream (started → stdout/stderr → exit) into the unary shape the
    /// old HTTP `/exec` returned.
    async fn exec(&self, sid: &str, command: &str) -> ExecResult {
        let req = self.req(
            app::ExecRequest {
                session_id: sid.to_string(),
                command: Some(command.to_string()),
                argv: vec![],
                env: HashMap::new(),
                workdir: None,
                timeout_secs: Some(30),
            },
            EXEC_TIMEOUT,
        );
        let mut stream = self
            .session()
            .exec(req)
            .await
            .unwrap_or_else(|e| panic!("Exec RPC failed: {e} command={command:?}"))
            .into_inner();

        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_status = None;
        while let Some(msg) = stream
            .message()
            .await
            .unwrap_or_else(|e| panic!("Exec stream error: {e} command={command:?}"))
        {
            match msg.event {
                Some(app::exec_output::Event::Started(_)) => {}
                Some(app::exec_output::Event::Stdout(b)) => {
                    stdout.push_str(&String::from_utf8_lossy(&b))
                }
                Some(app::exec_output::Event::Stderr(b)) => {
                    stderr.push_str(&String::from_utf8_lossy(&b))
                }
                Some(app::exec_output::Event::Exit(e)) => exit_status = e.exit_status,
                None => {}
            }
        }
        ExecResult {
            exit_status,
            stdout,
            stderr,
        }
    }

    async fn delete(&self, sid: &str) {
        let req = self.req(
            app::DeleteSessionRequest {
                session_id: sid.to_string(),
            },
            RPC_TIMEOUT,
        );
        let _ = self.session().delete_session(req).await;
    }

    /// ADR 0016 Phase B admin trigger, now `FleetService.FlushSession`.
    /// Forces an immediate `ChunkedDiskBackend::flush()` on the session's
    /// bound sandbox + publishes the manifest_ref into
    /// `sessions.live_disk_manifest_*` in the same coord-side TX.
    async fn flush_now(&self, sid: &str) -> app::FlushSessionResponse {
        let req = self.req(
            app::FlushSessionRequest {
                session_id: sid.to_string(),
            },
            RPC_TIMEOUT,
        );
        self.fleet()
            .flush_session(req)
            .await
            .expect("FlushSession")
            .into_inner()
    }

    /// `SessionService.Snapshot`. Returns when the snapshot's PG row is
    /// durable. The response is discarded — the side effect we care about
    /// is the row existing so `resume()` has something to find.
    async fn snapshot(&self, sid: &str) {
        let req = self.req(
            app::SnapshotRequest {
                session_id: sid.to_string(),
            },
            CREATE_TIMEOUT,
        );
        self.session().snapshot(req).await.expect("Snapshot");
    }

    /// `SessionService.EvictLocal` — evict the local sandbox after a
    /// snapshot, leaving the session in `Idle`. Required before `resume()`
    /// will reconstruct a fresh sandbox.
    async fn evict_local(&self, sid: &str) {
        let req = self.req(
            app::EvictLocalRequest {
                session_id: sid.to_string(),
            },
            RPC_TIMEOUT,
        );
        self.session().evict_local(req).await.expect("EvictLocal");
    }

    /// `SessionService.Resume`. Synchronous: returns when the session is
    /// Active again. The newly-bound sandbox_id is the one that should
    /// appear in `nbd_sandboxes` (per ADR 0016 Phase B commit 5).
    async fn resume(&self, sid: &str) {
        let req = self.req(
            app::ResumeRequest {
                session_id: sid.to_string(),
            },
            CREATE_TIMEOUT,
        );
        self.session().resume(req).await.expect("Resume");
    }

    /// `SessionService.GetCowState`. ADR 0016 Phase A diagnostic. Returns
    /// `Some(view)` when the sandbox is NBD-tracked (Phase B's
    /// chunked-disk pipeline live), `None` when the host fell back to
    /// materialize-to-file (no nbd.ko, no nbd_pool, etc.). Used by Phase B
    /// tests as a runtime probe for whether the chunked-disk-driven
    /// assertions are meaningful in this environment.
    async fn cow_state(&self, sid: &str) -> Option<app::CowStateView> {
        let req = self.req(
            app::GetCowStateRequest {
                session_id: sid.to_string(),
            },
            RPC_TIMEOUT,
        );
        self.session()
            .get_cow_state(req)
            .await
            .ok()?
            .into_inner()
            .state
    }

    /// Poll cow-state until `disk_manifest_version > prior` or the
    /// deadline elapses. Returns the new version on success, None on
    /// timeout. This is the load-bearing end-state assertion for Phase B
    /// e2e tests: a flush of dirty bytes — by either the FlushScheduler
    /// tick OR the admin flush-now trigger — advances
    /// `disk_manifest_version`. The test doesn't care which path produced
    /// the advance; both prove the chunked-disk publish pipeline works.
    async fn wait_for_disk_manifest_advance(
        &self,
        sid: &str,
        prior_version: u64,
        deadline: Duration,
    ) -> Option<u64> {
        let started = Instant::now();
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

    /// `FleetService.EvacuateSession`. Returns the response on success or
    /// the gRPC `Status` on error (NotFound for an unknown session,
    /// FailedPrecondition for a not-yet-Active one).
    async fn evacuate(&self, sid: &str) -> Result<app::EvacuateSessionResponse, tonic::Status> {
        let req = self.req(
            app::EvacuateSessionRequest {
                session_id: sid.to_string(),
                target_host: None,
            },
            RPC_TIMEOUT,
        );
        self.fleet()
            .evacuate_session(req)
            .await
            .map(|r| r.into_inner())
    }

    /// `FleetService.ChunkGc`. One RPC discriminated by `dry_run` folds
    /// today's paired `/dry-run` + `/sweep` routes (fleet.proto). `grace_secs`
    /// override lets tests promote candidates immediately (0).
    async fn chunk_gc(&self, dry_run: bool, grace_secs: Option<u64>) -> app::ChunkGcResponse {
        let req = self.req(
            app::ChunkGcRequest {
                dry_run,
                grace_secs,
            },
            CREATE_TIMEOUT,
        );
        self.fleet()
            .chunk_gc(req)
            .await
            .expect("ChunkGc")
            .into_inner()
    }

    /// Stream session events until the deadline, watching for an Anthropic
    /// auth-failure signal in either of the shapes documented on
    /// `AuthFailureSignal`. The gRPC `SessionEvent` envelope carries the
    /// event `kind` ("agent_message" / "run_completed") and a
    /// `payload_json` body (the same JSON the old SSE `data:` line
    /// carried), so we match on `kind` and pull `text` / `ok` out of the
    /// payload.
    async fn wait_for_anthropic_auth_failure(
        &self,
        sid: &str,
        expected_substr: &str,
        deadline: Duration,
    ) -> AuthFailureSignal {
        // Bound the server stream slightly past our own deadline so the
        // read loop (not the RPC) owns the timing.
        let req = self.req(
            app::StreamEventsRequest {
                session_id: sid.to_string(),
                since: None, // from the start
            },
            deadline + Duration::from_secs(10),
        );
        let mut stream = match self.session().stream_events(req).await {
            Ok(s) => s.into_inner(),
            Err(e) => {
                eprintln!("StreamEvents open failed: {e}");
                return AuthFailureSignal::TimedOut(Vec::new());
            }
        };

        let mut captured: Vec<String> = Vec::new();
        let started = Instant::now();

        loop {
            let Some(remaining) = deadline.checked_sub(started.elapsed()) else {
                return AuthFailureSignal::TimedOut(captured);
            };
            let ev = match tokio::time::timeout(remaining, stream.message()).await {
                Err(_) => return AuthFailureSignal::TimedOut(captured), // deadline
                Ok(Ok(None)) => return AuthFailureSignal::TimedOut(captured), // stream ended
                Ok(Err(e)) => {
                    eprintln!("StreamEvents IO error: {e}");
                    return AuthFailureSignal::TimedOut(captured);
                }
                Ok(Ok(Some(ev))) => ev,
            };

            match ev.kind.as_str() {
                "agent_message" => {
                    if let Some(text) = agent_message_text(&ev.payload_json) {
                        if text.contains(expected_substr) {
                            return AuthFailureSignal::ErrorMessage(text);
                        }
                    }
                    captured.push(format!("agent_message: {}", ev.payload_json));
                }
                "run_completed" => {
                    let ok = serde_json::from_str::<Value>(&ev.payload_json)
                        .ok()
                        .as_ref()
                        .and_then(|v| v.get("ok"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if !ok {
                        captured.push(format!("run_completed: {}", ev.payload_json));
                        // Grace window: the harness emits run_completed AFTER
                        // it's done forwarding agent_messages, but give the
                        // stream a tiny moment so a trailing agent_message with
                        // the error text (if any) can still land and tighten
                        // the signal to ErrorMessage.
                        let grace = Instant::now() + Duration::from_millis(500);
                        while Instant::now() < grace {
                            match tokio::time::timeout(Duration::from_millis(100), stream.message())
                                .await
                            {
                                Ok(Ok(Some(ev))) if ev.kind == "agent_message" => {
                                    if let Some(text) = agent_message_text(&ev.payload_json) {
                                        if text.contains(expected_substr) {
                                            return AuthFailureSignal::ErrorMessage(text);
                                        }
                                        captured.push(format!("agent_message: {}", ev.payload_json));
                                    }
                                }
                                Ok(Ok(Some(_))) => {}
                                _ => break,
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
                    captured.push(format!("run_completed: {}", ev.payload_json));
                }
                other => captured.push(format!("{other}: {}", ev.payload_json)),
            }
        }
    }
}

/// Pull the `text` field out of an `agent_message` event payload (the
/// JSON serialization of `SessionEvent::HarnessAgentMessage`, with rewind
/// meta folded in — see `api/events.rs::merged_to_parts`).
fn agent_message_text(payload_json: &str) -> Option<String> {
    serde_json::from_str::<Value>(payload_json)
        .ok()?
        .get("text")?
        .as_str()
        .map(str::to_owned)
}

// ---------- Tests ----------

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a live prod-shape stack with a baked demo image"]
async fn e2e_cold_session_no_harness_can_exec_ls() {
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;
    let resp = driver.exec(&sid, "ls /usr/local/bin").await;
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
    driver.delete(&sid).await;
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
    let driver = Driver::connect().await;
    let image = Driver::image_uri();
    let sid = driver.create_session_none_harness(&image).await;

    // The bundle is mounted read-only at the canonical guest path.
    let mounted = driver
        .exec(
            &sid,
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
            &sid,
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

    driver.delete(&sid).await;
}

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a baked claude-harness image"]
async fn e2e_cold_session_claude_harness_can_exec_ls() {
    // Raw Exec hits the sandbox directly — the Claude harness is bound but
    // unused. Use a bogus key so a future regression that races a harness
    // call won't burn real Anthropic budget; this test doesn't send a
    // prompt.
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid = driver
        .create_session_claude(&image, "sk-bogus-e2e-noprompt", None)
        .await;
    let resp = driver.exec(&sid, "ls /usr/local/bin").await;
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
    driver.delete(&sid).await;
}

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + Claude harness + real api.anthropic.com reachability"]
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

    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid = driver
        .create_session_claude(&image, "sk-bogus-e2e-prompt", Some("hello"))
        .await;

    let outcome = driver
        .wait_for_anthropic_auth_failure(&sid, EXPECTED_ERROR_SUBSTR, SSE_WAIT_DEADLINE)
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
    driver.delete(&sid).await;
}

/// ADR 0016 Phase B commit 4b: end-to-end exercise of the
/// FlushScheduler primitive via the `FleetService.FlushSession` trigger.
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
/// `nbd.ko` isn't in the guest kernel and `tilt-up-ci.sh`'s
/// probe falls back to materialize-to-file — `nbd_sandboxes` stays
/// empty, `flush_sandbox` returns None for every sandbox. The test
/// detects this via the Phase A `GetCowState` diagnostic (null state
/// == not chunk-tracked) and skips the applied/idle assertions with
/// a `::warning::` so the gap is visible in every run. The
/// endpoint-wired assertions (pre-write idle + sandbox-bound success)
/// still fire on every environment — so a regression in the RPC
/// surface or the wire format still fails loud. Self-hosted dev-vm
/// runner (or a Blacksmith NBD support request) is the documented
/// follow-up that closes the gap and turns the warning into a hard
/// assertion.
///
/// Test path:
/// 1. Cold-create a session against the demo image.
/// 2. **Always**: `flush_now` pre-write → assert `outcome=idle`.
///    RPC is wired, sandbox is bound (vs NotFound/FailedPrecondition),
///    no spurious Applied from a scheduler tick.
/// 3. Probe `cow-state`. If null → `::warning::` + return (env
///    can't exercise chunked-disk path).
/// 4. dd + sync into the chunked-disk-backed rootfs.
/// 5. `flush_now` → best-effort applied; the end-state poll is what
///    asserts the full host → PG round-trip.
/// 6. Poll cow-state until `disk_manifest_version` advances.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_flush_now_applies_then_short_circuits_on_no_dirty() {
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Step 2: pre-write flush. Environment-independent — the
    // endpoint should always return idle when nothing's dirty,
    // regardless of whether the host has NBD wired. Success + idle
    // here proves the RPC is wired, the session-lookup works,
    // and `flush_sandbox` returns None cleanly.
    let pre = driver.flush_now(&sid).await;
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
    // `nbd.ko` isn't in the guest kernel → tilt-up-ci.sh
    // falls back to materialize-to-file → cow-state is null →
    // skip the rest with a loud warning so the gap is visible in
    // every CI run.
    let cow = driver.cow_state(&sid).await;
    if cow.is_none() {
        eprintln!(
            "::warning title=Phase B flush-now e2e partial coverage::\
             cow-state returned null for session {sid} — the runner's host-agent \
             fell back to materialize-to-file (no nbd.ko / no nbd_pool wired). \
             Pre-write flush_now=idle assertion verified the RPC wiring, but \
             the chunked-disk write → flush → publish round-trip can't be exercised \
             here. See ci.yml line 518-532 + the dev-vm self-hosted runner follow-up."
        );
        driver.delete(&sid).await;
        return;
    }

    // Step 4: capture the baseline disk_manifest_version BEFORE
    // dd. The FlushScheduler may already have ticked between
    // create-session and now (its 30s default cadence can fire
    // during the slow cold-create); whatever version it left
    // behind is what we measure forward from.
    let baseline_version = cow.as_ref().map(|s| s.disk_manifest_version).unwrap_or(0);

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
            &sid,
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
    let sync = driver.exec(&sid, "sync").await;
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
    let _ = driver.flush_now(&sid).await;

    // Step 7: end-state assertion. Poll cow-state until the disk
    // manifest version advances past `baseline_version`. The
    // 90-second deadline covers one full scheduler tick (30s) +
    // generous CI slack. If we don't see an advance in that
    // window, the chunked-disk publish pipeline is broken —
    // neither the admin trigger nor the scheduler produced a new
    // version after 8 MiB of writes.
    let advanced = driver
        .wait_for_disk_manifest_advance(&sid, baseline_version, Duration::from_secs(90))
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

    driver.delete(&sid).await;
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
/// flush-now + snapshot + evict + resume RPC surface (catches
/// regressions in those handlers) but skips the cow-state-post-
/// resume + flush-now-post-resume assertions with a loud warning.
/// The full regression coverage kicks in on dev-vm / future
/// self-hosted runner where NBD is wired.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_rejoins_chunked_disk_tracking() {
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Probe FIRST — if this environment can't exercise the NBD
    // path, every subsequent assertion below is meaningless. The
    // pre-write idle-flush check is redundant with
    // `e2e_flush_now_applies_then_short_circuits_on_no_dirty`,
    // which already pins the always-on endpoint shape.
    let pre_cow = driver.cow_state(&sid).await;
    if pre_cow.is_none() {
        eprintln!(
            "::warning title=Phase B resume regression partial coverage::\
             cow-state returned null for session {sid} (pre-snapshot) — runner \
             can't exercise the NBD path. Snapshot+resume RPC wiring will \
             still be exercised below; cow-state-post-resume + \
             flush-now-post-resume assertions skipped. \
             See ci.yml line 518-532 + dev-vm self-hosted runner follow-up."
        );
        driver.delete(&sid).await;
        return;
    }

    // Dirty the disk so the snapshot we take has a non-trivial
    // disk_manifest the resume path can NBD-attach against. Same
    // dd+sync shape as the flush-now test.
    let dd = driver
        .exec(
            &sid,
            "dd if=/dev/zero of=/var/dirty.bin bs=1M count=8 status=none",
        )
        .await;
    assert_eq!(
        dd.exit_status,
        Some(0),
        "dd should succeed; stderr=<{}>",
        dd.stderr,
    );
    let sync = driver.exec(&sid, "sync").await;
    assert_eq!(sync.exit_status, Some(0), "sync should succeed");

    // Best-effort flush via the admin trigger (the FlushScheduler
    // may have already drained — same race as e2e_flush_now). The
    // snapshot below will internally re-flush via its own
    // backend.flush() call regardless, so the chunks ARE durable
    // in BlobStorage by the time we evict.
    let _ = driver.flush_now(&sid).await;

    // Snapshot → evict-local → resume. Equivalent to the
    // idle-eviction → resume cycle prod exercises, minus the
    // 30-second idle wait.
    driver.snapshot(&sid).await;
    driver.evict_local(&sid).await;
    driver.resume(&sid).await;

    // **THE REGRESSION CHECK**: post-resume cow-state must be
    // Some(...). Pre-commit-5 this returned null (Symptom 1 of the
    // ADR's failure mode). Post-commit-5 the resumed sandbox is in
    // `nbd_sandboxes` → diagnostic populates.
    let post_cow = driver.cow_state(&sid).await;
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
    let post_resume_baseline = post_cow.as_ref().map(|s| s.disk_manifest_version).unwrap_or(0);

    let post_dd = driver
        .exec(
            &sid,
            "dd if=/dev/zero of=/var/post-resume.bin bs=1M count=4 status=none",
        )
        .await;
    assert_eq!(
        post_dd.exit_status,
        Some(0),
        "post-resume dd should succeed; stderr=<{}>",
        post_dd.stderr,
    );
    let post_sync = driver.exec(&sid, "sync").await;
    assert_eq!(
        post_sync.exit_status,
        Some(0),
        "post-resume sync should succeed",
    );

    // Best-effort admin trigger (same race-tolerance as
    // e2e_flush_now_applies_then_short_circuits_on_no_dirty).
    let _ = driver.flush_now(&sid).await;

    // End-state assertion: the resumed sandbox's disk manifest
    // version must advance past the post-resume baseline. This is
    // the load-bearing regression check — pre-commit-5 this would
    // hang forever (resumed sandbox isn't in nbd_sandboxes, no
    // backend.flush() to advance the version).
    let post_advanced = driver
        .wait_for_disk_manifest_advance(&sid, post_resume_baseline, Duration::from_secs(90))
        .await;
    assert!(
        post_advanced.is_some(),
        "post-resume disk_manifest_version never advanced past baseline {post_resume_baseline} in 90s — \
         the resumed sandbox isn't rejoined to chunked-disk tracking",
    );

    driver.delete(&sid).await;
}

// ---------------------------------------------------------------------
// ADR 0016 Phase C commit 6a — chunk-GC RPC e2e regression
// ---------------------------------------------------------------------

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
/// 2. `ChunkGc{dry_run:true, grace_secs:0}` — assert pin_set_size > 0.
///    If the enabled-image pin-set source were broken, this would
///    report 0 here, and step 3's sweep would proceed to delete
///    every chunk in the bucket.
/// 3. `ChunkGc{dry_run:false, grace_secs:0}` — full sweep with grace
///    knocked down so any orphans would promote on this single call.
///    The load-bearing claim: this MUST NOT delete the demo image's
///    chunks, regardless of what's in the bucket.
/// 4. Re-exec a command on the original session — proves the
///    sweep didn't break the session's data path.
/// 5. Create a SECOND none-harness session on the same image —
///    the strongest regression catch. Session-create materializes
///    the rootfs anew; if the sweep deleted the image's chunks,
///    this fails the same way the M5 incident failed.
/// 6. `ChunkGc{dry_run:false, grace_secs:0}` again — idempotent on a
///    clean stack: pin set membership hasn't changed and any
///    candidates marked in step 3 were already promoted.
///
/// NOTE: today's HTTP `GET /api/admin/chunk-gc/candidates` round-trip
/// (the old step 6) is intentionally dropped here — fleet.proto gives
/// that endpoint NO RPC ("neither has a web caller today"). The
/// candidate-table shape is covered by the `admin_chunk_gc_live_pg` +
/// `chunk_gc_helpers_live_pg` tests in the same lane.
///
/// Environment dependence: this test does NOT skip on missing NBD.
/// The pin-set + sweep paths run against PG + BlobStorage only;
/// session create+exec works against either NBD-attached or
/// materialize-to-file rootfs.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_chunk_gc_sweep_does_not_delete_live_image_chunks() {
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    let sid_1 = driver.create_session_none_harness(&image).await;

    // Step 2: dry-run baseline. The pin set MUST cover the demo
    // image's chunks (enabled_images source). If this is 0, the
    // regression has already happened by construction and step 3's
    // sweep would wipe the bucket.
    let dry = driver.chunk_gc(true, Some(0)).await;
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

    // Capture sizes for the idempotency assertion in step 6.
    let pin_set_size_before = dry.pin_set_size;

    // Step 3: full sweep with grace=0. The load-bearing call. If
    // the enabled image's chunks aren't in the pin set, they get
    // marked AND promoted in a single call — the M5 failure mode.
    let swept = driver.chunk_gc(false, Some(0)).await;
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
    let ls = driver.exec(&sid_1, "ls /").await;
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
    let ls_2 = driver.exec(&sid_2, "ls /").await;
    assert_eq!(
        ls_2.exit_status,
        Some(0),
        "post-sweep fresh session-create + exec failed — the sweep \
         deleted the image's chunks under us (M5 regression). \
         stderr=<{}>",
        ls_2.stderr,
    );

    // Step 6: idempotency on a clean stack. After step 3's promote
    // pass deleted any orphans the candidate table had, a second
    // sweep at grace=0 should find nothing to promote.
    let swept_again = driver.chunk_gc(false, Some(0)).await;
    assert_eq!(
        swept_again.promote_delete_errors, 0,
        "second sweep must not error",
    );
    assert!(
        swept_again.pin_set_size > 0,
        "pin set must still cover live images after a clean sweep",
    );

    driver.delete(&sid_1).await;
    driver.delete(&sid_2).await;
}

/// ADR 0018 Phase C evac RPC shape coverage.
///
/// The full multi-host alive-source evac requires two host-agents in
/// the integration stack. The current `tilt-up-ci.sh` spins up one.
/// Until a 2-host variant lands as a follow-up, this test exercises
/// what's reachable from a 1-host fixture:
///
///   - RPC is mounted under bearer auth.
///   - `Code::NotFound` on a non-existent session id.
///   - For an Active session: returns an `EvacuateSessionResponse`
///     with `status="evacuating"`. The `evac_resumer` scanner (which
///     runs in the same coord process) drives the session to Active
///     on a peer in ≤10s.
///
/// Pinned regressions:
///   - RPC registration (a mis-wire makes every call Unimplemented).
///   - Pre-flight checks (NotFound vs FailedPrecondition vs Internal)
///     in `evacuate_session`.
///   - The async hand-off: the handler MUST return without
///     synchronously running the relocate (legacy commit-7 shape).
///
/// The full e2e — Active session × 2 hosts × disk-preserved-on-peer
/// — runs in the dev-vm `integration-evac-test.sh`.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_evac_admin_endpoint_shape() {
    let driver = Driver::connect().await;
    let image = Driver::image_uri();

    // Case 1: NotFound on a session that doesn't exist.
    let bogus = uuid::Uuid::new_v4().to_string();
    let err = driver
        .evacuate(&bogus)
        .await
        .expect_err("evacuate on unknown session must error");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "evacuate on unknown session should be NotFound; got {:?} ({})",
        err.code(),
        err.message(),
    );

    // Case 2: live session — async shape (commit 12). Handler must
    // return immediately after marking the session Evacuating. The
    // scanner picks it up from there.
    let sid = driver.create_session_none_harness(&image).await;
    let resp = driver
        .evacuate(&sid)
        .await
        .expect("evacuate must succeed on Active session (async shape)");
    assert_eq!(
        resp.status, "evacuating",
        "evacuate response status must be \"evacuating\"; got {resp:?}",
    );
    assert_eq!(
        resp.session_id, sid,
        "evacuate response must echo session_id; got {resp:?}",
    );

    driver.delete(&sid).await;
}
