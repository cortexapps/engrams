//! End-to-end tests against a live prod-shape stack (coord + host-agent
//! + Firecracker + Postgres + chunked-OCI registry + fake-gcs).
//!
//! The existing integration tests stop one layer short of the coord app
//! surface: `e2e_harness.rs` and `e2e_shell.rs` drive `PooledBackend`
//! directly; `ha_listener.rs` and friends use an in-proc `AppState`.
//! That left the coord app-gRPC → gRPC → host-agent → FC path uncovered,
//! which is how prod session 8725648d's empty-Binary-frame bug shipped.
//!
//! ADR 0051 made the coordinator a gRPC-only control plane: the web-facing
//! axum REST routes were deleted, and the orchestrator-facing app surface
//! is now `engram.app.v1` gRPC (SessionService / FleetService /
//! ImageService / SecretService / ShellRelay), served on
//! `ENGRAM_APP_GRPC_ADDR` (default `127.0.0.1:50061`) and fail-closed-authed
//! by a static bearer in `ENGRAM_APP_GRPC_TOKENS`. This harness drives that
//! surface directly via a tonic `SessionServiceClient` + `FleetServiceClient`
//! — exactly how the orchestrator (and `engram-cli`) drive the coordinator —
//! preserving every assertion the old `reqwest` HTTP harness made.
//!
//! HTTP→gRPC RPC mapping (see the per-method docs below):
//!   POST /api/v1/sessions                         → SessionService::CreateSession
//!   POST /api/v1/sessions/:id/exec                → SessionService::Exec (streaming, collected)
//!   DELETE /api/v1/sessions/:id                   → SessionService::DeleteSession
//!   POST /api/v1/sessions/:id/snapshot            → SessionService::Snapshot
//!   DELETE /api/v1/sessions/:id/local             → SessionService::EvictLocal
//!   POST /api/v1/sessions/:id/resume              → SessionService::Resume
//!   GET /api/v1/sessions/:id                       → SessionService::GetSession
//!   GET /api/v1/sessions/:id/cow-state            → SessionService::GetCowState
//!   GET /api/v1/sessions/:id/events               → SessionService::StreamEvents
//!   GET /api/v1/hosts                              → FleetService::ListHosts
//!   POST /api/v1/admin/sessions/:id/flush-now     → FleetService::FlushSession
//!   POST /api/v1/admin/sessions/:id/evacuate      → FleetService::EvacuateSession
//!   POST /api/v1/admin/chunk-gc/{dry-run,sweep}   → FleetService::ChunkGc (dry_run flag)
//!
//! TWO HTTP routes the ADR 0051 commit DELETED with NO gRPC equivalent
//! (verified absent from session.proto / fleet.proto and the grpc_app
//! impls). They are handled faithfully through the closest gRPC primitive,
//! and the divergence is documented LOUDLY on each affected test:
//!   - POST /admin/sessions/:id/evict-idle  → ported to Snapshot + EvictLocal
//!     (the same Active→Idle lifecycle the manual handlers drive; the
//!     `idle_evictor::evict_idle_session` *primitive* itself has no app RPC).
//!   - POST /admin/sessions/:id/teleport {target_host_id} → ported to
//!     FleetService::EvacuateSession. EvacuateSession relocates to ANY
//!     non-source peer (its `target_host` field is explicitly IGNORED by
//!     the handler). In the two-host teleport stack there is exactly one
//!     peer, so "evacuate to any peer" is observationally identical to
//!     "teleport to the one other host" — `host_id == dest` still holds.
//!
//! All tests are `#[ignore]`'d and gated by env vars. The CI lane
//! `test-e2e-stack` in `.github/workflows/ci.yml` brings up the stack
//! (`tilt-up-ci.sh` + `integration-bake-demo.sh`), runs these tests via
//! `cargo nextest --run-ignored`, and tears down on completion. ADR 0021
//! P1.5 retired the separate `harness add` step — the harness is baked into
//! the image at `/opt/engram/harness/` and the coord reads the launch
//! contract from `manifest.toml`.

use std::collections::HashMap;
use std::time::Duration;

use engram_protocol::app;
use futures::StreamExt;
use serde_json::Value;
use tonic::codegen::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Channel;
use tonic::Code;

use engram_core::SessionId;

/// Per-RPC timeout for the test's tonic channel. Cold `CreateSession` on
/// CI runs ~120s on a fresh chunk cache (FC boot + first NBD page-ins),
/// so this needs to clear that with a little headroom.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// How long the auth-failure test waits for either an `agent_message` or a
/// `run_completed(ok=false)` event after session-create returns. Claude's
/// stream-json round-trip through the harness + egress proxy +
/// api.anthropic.com 401 is typically <10s, but CI networking adds latency.
/// 180s = "if no signal arrives in this long, Claude is genuinely hung (not
/// just slow)" — a strong failure signal worth panicking on.
const SSE_WAIT_DEADLINE: Duration = Duration::from_secs(180);

/// Auth-failure observation outcome.
///
/// The Claude harness today emits `run_completed { ok: false }` when Claude
/// exits non-zero, but the user's hypothesis is that Claude's stream-json
/// output *also* carries a parseable error message that surfaces as an
/// `agent_message` event. We don't yet know what the Anthropic 401 JSON
/// looks like when it reaches the dashboard — the first green CI run will
/// tell us. Until then this enum lets the test pass on either signal and
/// prints captured events on the fallback path so the next iteration can
/// tighten the assertion.
#[derive(Debug)]
#[allow(dead_code)] // variants used only when the test runs (gated)
enum AuthFailureSignal {
    /// An `agent_message` arrived containing the expected substring (or any
    /// future-tightened equivalent). The string is the full message text so
    /// a regression that drops the substring surfaces clearly.
    ErrorMessage(String),
    /// No structured error message, but `run_completed.ok == false` arrived —
    /// the chain ran and Anthropic rejected the auth. Captured events are
    /// printed to stderr to inform the next tightening pass.
    RunCompletedNotOk,
    /// Neither signal in the deadline. Carries the captured `(kind,
    /// payload_json)` pairs for diagnostic output.
    TimedOut(Vec<(String, String)>),
}

/// tonic interceptor that stamps `Authorization: Bearer <token>` onto every
/// outbound request — the only way to set per-call metadata short of
/// hand-building each request. Mirrors `grpc_app.rs`'s `bearer()` helper
/// and `engram-cli`'s `with_bearer`.
#[derive(Clone)]
struct BearerInterceptor {
    token: Option<String>,
}

impl Interceptor for BearerInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(tok) = &self.token {
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer {tok}")
                    .parse()
                    .expect("token bytes are valid ASCII"),
            );
        }
        Ok(req)
    }
}

type AuthedChannel = InterceptedService<Channel, BearerInterceptor>;
type SessionClient = app::session_service_client::SessionServiceClient<AuthedChannel>;
type FleetClient = app::fleet_service_client::FleetServiceClient<AuthedChannel>;

/// gRPC driver against the app surface. Holds a connected, bearer-stamped
/// channel; each accessor builds a fresh service client over it (tonic
/// clients are cheap clones that share the underlying connection pool).
struct Driver {
    channel: AuthedChannel,
}

impl Driver {
    /// Connect to the app-gRPC surface from env. `ENGRAM_E2E_GRPC_ADDR` is the
    /// dial URI (e.g. `http://127.0.0.1:50061`); `ENGRAM_E2E_GRPC_TOKEN` is the
    /// bearer that must be in the coordinator's `ENGRAM_APP_GRPC_TOKENS`
    /// allow-list (the coord fails closed without it).
    async fn from_env() -> Self {
        let addr = std::env::var("ENGRAM_E2E_GRPC_ADDR")
            .expect("ENGRAM_E2E_GRPC_ADDR must be set (e.g. http://127.0.0.1:50061)");
        let token = std::env::var("ENGRAM_E2E_GRPC_TOKEN").ok();
        let endpoint = tonic::transport::Endpoint::from_shared(addr.clone())
            .expect("parse ENGRAM_E2E_GRPC_ADDR")
            // Per-RPC deadline: matches the old reqwest client timeout so a
            // hung cold-create surfaces as a DeadlineExceeded rather than
            // wedging the whole nextest run.
            .timeout(DEFAULT_TIMEOUT);
        let channel = endpoint
            .connect()
            .await
            .expect("dial app gRPC at ENGRAM_E2E_GRPC_ADDR");
        let interceptor = BearerInterceptor { token };
        Self {
            channel: InterceptedService::new(channel, interceptor),
        }
    }

    fn sessions(&self) -> SessionClient {
        app::session_service_client::SessionServiceClient::new(self.channel.clone())
    }

    fn fleet(&self) -> FleetClient {
        app::fleet_service_client::FleetServiceClient::new(self.channel.clone())
    }

    fn image_uri() -> String {
        std::env::var("ENGRAM_E2E_IMAGE_URI").expect(
            "ENGRAM_E2E_IMAGE_URI must be set — the upstream CI step that ran \
             integration-bake-demo.sh writes it to $GITHUB_ENV",
        )
    }

    /// ADR 0021 P1.3: drive the image as a pure dev VM. `mode = dev_vm` tells
    /// coord to skip `resolve_harness` and pass an empty-argv `AgentSpec` to
    /// the backend; agentd hits the readiness-probe branch and never execs the
    /// harness binary even if it's sitting in the rootfs.
    ///
    /// → `SessionService::CreateSession`.
    async fn create_session_none_harness(&self, image: &str) -> SessionId {
        let resp = self
            .sessions()
            .create_session(app::CreateSessionRequest {
                image_uri: image.to_string(),
                mode: "dev_vm".into(),
                prompt: None,
                harness_secret_id: None,
                secrets: HashMap::new(),
            })
            .await
            .expect("CreateSession (dev_vm)")
            .into_inner();
        resp.session_id.parse().expect("decode session_id")
    }

    /// ADR 0021 P1.3: drive the image's baked harness. `mode = agent`. The
    /// image referenced by `ENGRAM_E2E_IMAGE_URI` must carry a `[harness]
    /// builtin = "claude"` block. The per-request `ANTHROPIC_API_KEY` is
    /// passed via the `secrets` map (honored under `SecretMode::Literal`),
    /// mirroring the old HTTP body's `"secrets"` object.
    ///
    /// → `SessionService::CreateSession`.
    async fn create_session_claude(
        &self,
        image: &str,
        api_key: &str,
        prompt: Option<&str>,
    ) -> SessionId {
        let mut secrets = HashMap::new();
        secrets.insert("ANTHROPIC_API_KEY".to_string(), api_key.to_string());
        let resp = self
            .sessions()
            .create_session(app::CreateSessionRequest {
                image_uri: image.to_string(),
                mode: "agent".into(),
                prompt: prompt.map(str::to_string),
                harness_secret_id: None,
                secrets,
            })
            .await
            .expect("CreateSession (claude)")
            .into_inner();
        resp.session_id.parse().expect("decode session_id")
    }

    /// Run a shell command in the session and collect the streamed output
    /// into a single `ExecResult`. The proto `Exec` RPC frames output as
    /// `started` → `stdout|stderr` bytes → `exit`; this reassembles the
    /// degenerate unary case the old `POST /exec` returned.
    ///
    /// → `SessionService::Exec` (server-streaming, collected).
    async fn exec(&self, sid: SessionId, command: &str) -> ExecResult {
        let stream = self
            .sessions()
            .exec(app::ExecRequest {
                session_id: sid.to_string(),
                command: Some(command.to_string()),
                argv: Vec::new(),
                env: HashMap::new(),
                workdir: None,
                timeout_secs: Some(30),
            })
            .await
            .unwrap_or_else(|e| panic!("Exec rpc open failed: {e} command={command:?}"))
            .into_inner();
        collect_exec(stream, command).await
    }

    /// → `SessionService::DeleteSession`. Idempotent; errors are swallowed
    /// (the old HTTP delete discarded its status too).
    async fn delete(&self, sid: SessionId) {
        let _ = self
            .sessions()
            .delete_session(app::DeleteSessionRequest {
                session_id: sid.to_string(),
            })
            .await;
    }

    /// `Snapshot` — returns when the snapshot's PG row is durable. We discard
    /// the response; the side effect we care about is the row existing so
    /// `resume()` has something to find.
    ///
    /// → `SessionService::Snapshot`.
    async fn snapshot(&self, sid: SessionId) {
        self.sessions()
            .snapshot(app::SnapshotRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("Snapshot");
    }

    /// `EvictLocal` — evict the local sandbox after a snapshot, leaving the
    /// session in `Idle`. Required before `resume()` will reconstruct a fresh
    /// sandbox.
    ///
    /// → `SessionService::EvictLocal`.
    async fn evict_local(&self, sid: SessionId) {
        self.sessions()
            .evict_local(app::EvictLocalRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("EvictLocal");
    }

    /// `Resume`. Synchronous: returns when the session is Active again.
    ///
    /// → `SessionService::Resume`.
    async fn resume(&self, sid: SessionId) {
        self.sessions()
            .resume(app::ResumeRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("Resume");
    }

    /// ADR 0016 Phase B commit 4a admin trigger. Forces an immediate
    /// `ChunkedDiskBackend::flush()` on the session's bound sandbox +
    /// publishes the manifest_ref into `sessions.live_disk_manifest_*` in the
    /// same coord-side TX.
    ///
    /// → `FleetService::FlushSession`.
    async fn flush_now(&self, sid: SessionId) -> app::FlushSessionResponse {
        self.fleet()
            .flush_session(app::FlushSessionRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("FlushSession")
            .into_inner()
    }

    /// `GetCowState`. ADR 0016 Phase A diagnostic. Returns `Some(json)` when
    /// the sandbox is NBD-tracked (Phase B's chunked-disk pipeline live),
    /// `None` when the host fell back to materialize-to-file. Used by Phase B
    /// tests as a runtime probe for whether the chunked-disk-driven
    /// assertions are meaningful in this environment.
    ///
    /// Returns the proto `CowStateView` directly (the field accessors below
    /// stand in for the old JSON `.get("...")` probes).
    ///
    /// → `SessionService::GetCowState`.
    async fn cow_state(&self, sid: SessionId) -> Option<app::CowStateView> {
        let resp = self
            .sessions()
            .get_cow_state(app::GetCowStateRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("GetCowState")
            .into_inner();
        resp.state
    }

    /// Poll `GetCowState` until `disk_manifest_version > prior` or the
    /// deadline elapses. Returns the new version on success, None on timeout.
    /// This is the load-bearing end-state assertion for Phase B e2e tests: a
    /// flush of dirty bytes — by either the FlushScheduler tick OR the admin
    /// flush-now trigger — advances `disk_manifest_version`.
    async fn wait_for_disk_manifest_advance(
        &self,
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

    /// `GetSession` → the proto `Session`. Black-box read: the lifecycle
    /// tests observe `status` (snake_case `SessionState` — "active" / "idle"
    /// / "evacuating") and `host_id` exactly as a real client would. A pure
    /// read; it does not bump the session's activity clock.
    ///
    /// → `SessionService::GetSession`.
    async fn get_session(&self, sid: SessionId) -> app::Session {
        self.sessions()
            .get_session(app::GetSessionRequest {
                session_id: sid.to_string(),
            })
            .await
            .expect("GetSession")
            .into_inner()
            .session
            .expect("GetSession response must carry a session")
    }

    /// The session's `status` string (snake_case `SessionState`).
    async fn session_status(&self, sid: SessionId) -> String {
        self.get_session(sid).await.status
    }

    /// The session's bound `host_id` (`None` while Idle / unbound). Used by
    /// the teleport test to assert the session relocated.
    async fn session_host_id(&self, sid: SessionId) -> Option<String> {
        self.get_session(sid).await.host_id
    }

    /// `ListHosts` → the registered hosts' ids. The teleport test uses this to
    /// discover a peer to relocate onto.
    ///
    /// → `FleetService::ListHosts`.
    async fn list_host_ids(&self) -> Vec<String> {
        let resp = self
            .fleet()
            .list_hosts(app::ListHostsRequest::default())
            .await
            .expect("ListHosts")
            .into_inner();
        resp.hosts.into_iter().map(|h| h.id).collect()
    }

    /// Poll `GetSession` until `status == want` or the deadline elapses.
    /// Returns true on match. Used to observe Active→Idle (eviction) and
    /// Idle/Evacuating→Active (resume / teleport rejoin) through the public
    /// API alone.
    async fn wait_for_status(&self, sid: SessionId, want: &str, deadline: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if self.session_status(sid).await == want {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        false
    }

    /// Stream session events until the deadline, watching for an Anthropic
    /// auth-failure signal in either of the shapes documented on
    /// `AuthFailureSignal`.
    ///
    /// Over gRPC the event wire is `app::SessionEvent { idx, kind,
    /// payload_json }`: the typed event ENVELOPE. `kind` is the discriminant
    /// ("agent_message" / "run_completed"); `payload_json` is the event
    /// payload object (the `type` tag stripped into `kind`). We match on
    /// `kind` and pull `.text` / `.ok` out of the parsed payload.
    ///
    /// → `SessionService::StreamEvents` (server-streaming).
    async fn wait_for_anthropic_auth_failure(
        &self,
        sid: SessionId,
        expected_substr: &str,
        deadline: Duration,
    ) -> AuthFailureSignal {
        // `since` UNSET = replay from the start, then tail (the old HTTP
        // `?since=-1` sentinel). proto3 default 0 would silently skip idx 0,
        // hence None (not Some(0)).
        let stream = self
            .sessions()
            .stream_events(app::StreamEventsRequest {
                session_id: sid.to_string(),
                since: None,
            })
            .await
            .expect("StreamEvents open")
            .into_inner();

        let mut captured: Vec<(String, String)> = Vec::new();
        let started = std::time::Instant::now();
        tokio::pin!(stream);

        loop {
            let Some(remaining) = deadline.checked_sub(started.elapsed()) else {
                return AuthFailureSignal::TimedOut(captured);
            };
            let next = tokio::time::timeout(remaining, stream.next()).await;
            let ev = match next {
                Err(_) => return AuthFailureSignal::TimedOut(captured),
                Ok(None) => return AuthFailureSignal::TimedOut(captured),
                Ok(Some(Err(status))) => {
                    eprintln!("StreamEvents stream error: {status}");
                    return AuthFailureSignal::TimedOut(captured);
                }
                Ok(Some(Ok(ev))) => ev,
            };

            let payload: Value = serde_json::from_str(&ev.payload_json).unwrap_or(Value::Null);

            // Signal 1: an assistant agent_message carrying the error text.
            if ev.kind == "agent_message" {
                if let Some(text) = payload.get("text").and_then(Value::as_str) {
                    if text.contains(expected_substr) {
                        return AuthFailureSignal::ErrorMessage(text.to_string());
                    }
                }
            }

            // Signal 2: run_completed { ok: false }. Don't return
            // immediately — give the stream a tiny grace so a trailing
            // agent_message with the error text (if any) can land. The
            // harness emits run_completed AFTER it's done forwarding
            // agent_messages.
            if ev.kind == "run_completed"
                && payload.get("ok").and_then(Value::as_bool) == Some(false)
            {
                captured.push((ev.kind.clone(), ev.payload_json.clone()));
                let grace = Duration::from_millis(500);
                let grace_deadline = std::time::Instant::now() + grace;
                while std::time::Instant::now() < grace_deadline {
                    let rem = grace_deadline.saturating_duration_since(std::time::Instant::now());
                    match tokio::time::timeout(rem, stream.next()).await {
                        Ok(Some(Ok(trailing))) => {
                            let p: Value =
                                serde_json::from_str(&trailing.payload_json).unwrap_or(Value::Null);
                            if trailing.kind == "agent_message" {
                                if let Some(text) = p.get("text").and_then(Value::as_str) {
                                    if text.contains(expected_substr) {
                                        return AuthFailureSignal::ErrorMessage(text.to_string());
                                    }
                                }
                            }
                            captured.push((trailing.kind, trailing.payload_json));
                        }
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

            captured.push((ev.kind, ev.payload_json));
        }
    }

    /// ADR 0016 Phase C: chunk-GC dry-run. Folds the old
    /// `POST /api/admin/chunk-gc/dry-run` route into `ChunkGc { dry_run:
    /// true }`.
    ///
    /// → `FleetService::ChunkGc`.
    async fn chunk_gc_dry_run(&self, grace_secs: Option<u64>) -> app::ChunkGcResponse {
        self.fleet()
            .chunk_gc(app::ChunkGcRequest {
                dry_run: true,
                grace_secs,
            })
            .await
            .expect("ChunkGc dry_run")
            .into_inner()
    }

    /// ADR 0016 Phase C: chunk-GC full sweep. Folds the old
    /// `POST /api/admin/chunk-gc/sweep` route into `ChunkGc { dry_run:
    /// false }`. (proto3 footgun: `dry_run: false` is a LIVE sweep with
    /// deletions — set deliberately, which the bool literal here does.)
    ///
    /// → `FleetService::ChunkGc`.
    async fn chunk_gc_sweep(&self, grace_secs: Option<u64>) -> app::ChunkGcResponse {
        self.fleet()
            .chunk_gc(app::ChunkGcRequest {
                dry_run: false,
                grace_secs,
            })
            .await
            .expect("ChunkGc sweep")
            .into_inner()
    }
}

/// Collected result of a streamed `Exec` RPC — the unary shape the old HTTP
/// `POST /exec` returned (exit status + concatenated stdout/stderr).
struct ExecResult {
    exit_status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Drain an `Exec` output stream into an `ExecResult`. Frames:
///   started → (stdout|stderr bytes)* → exit
/// stdout/stderr are intentionally bytes on the wire; we stringify lossily
/// at the edge exactly as the old API did.
async fn collect_exec(mut stream: tonic::Streaming<app::ExecOutput>, command: &str) -> ExecResult {
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut exit_status: Option<i32> = None;
    let mut saw_exit = false;

    while let Some(item) = stream.next().await {
        let frame = item.unwrap_or_else(|e| panic!("Exec stream error: {e} command={command:?}"));
        match frame.event {
            Some(app::exec_output::Event::Started(_)) => {}
            Some(app::exec_output::Event::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(app::exec_output::Event::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(app::exec_output::Event::Exit(exit)) => {
                exit_status = exit.exit_status;
                saw_exit = true;
            }
            None => {}
        }
    }
    assert!(
        saw_exit,
        "Exec stream ended without an exit frame; command={command:?}"
    );

    ExecResult {
        exit_status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

// ---------- Tests ----------

#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a live prod-shape stack with a baked demo image"]
async fn e2e_cold_session_no_harness_can_exec_ls() {
    let driver = Driver::from_env().await;
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
    let driver = Driver::from_env().await;
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
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a Claude harness baked into the demo image"]
async fn e2e_cold_session_claude_harness_can_exec_ls() {
    // Raw exec hits the sandbox directly — the Claude harness is bound but
    // unused. Use a bogus key so a future regression that races a harness
    // call won't burn real Anthropic budget; this test doesn't send a prompt.
    let driver = Driver::from_env().await;
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
    // Tightened from the iteration-1 stub after run 26338080977 showed the
    // actual shape. Claude CLI surfaces Anthropic's 401 as an assistant-role
    // agent_message with literal text:
    //
    //   "Invalid API key · Fix external API key"
    //
    // (the `·` is a middle dot, U+00B7; we only assert on the ASCII prefix).
    // The harness's `run_completed.ok` is actually `true` on this path
    // because the Claude CLI process itself exited cleanly — the auth error
    // lives entirely in the stream-json output it printed before exit. That's
    // why this test watches `agent_message` text, not the `ok` flag.
    const EXPECTED_ERROR_SUBSTR: &str = "Invalid API key";

    let driver = Driver::from_env().await;
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
            // Acceptable on the first iteration: the chain ran and failed
            // cleanly. The stderr dump from wait_for_anthropic_auth_failure
            // tells us what to tighten to.
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

/// Whether this environment is REQUIRED to have ≥2 hosts (the teleport
/// scenario). Set `ENGRAM_EXPECT_TWO_HOSTS=1` on the CI lane that boots the
/// two-host stack (`ENGRAM_INTEG_TWO_HOSTS=1`) so a single-host environment
/// becomes a hard failure instead of a silent skip.
fn two_hosts_required() -> bool {
    matches!(
        std::env::var("ENGRAM_EXPECT_TWO_HOSTS").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// ADR 0016 Phase B commit 4b: end-to-end exercise of the FlushScheduler
/// primitive via the admin `flush-now` endpoint (now `FleetService::
/// FlushSession`). Explicit-trigger counterpart to the scheduler's 30s
/// implicit cadence; the only way to verify the chunked-disk-write →
/// `backend.flush()` → coord `update_live_disk_manifest` → PG row round-trip
/// in CI without sleeping a full cadence.
///
/// **Environment dependence** (per `[fc_tests_run_in_ci]`): the chunked-disk
/// write assertions only fire when the host has `nbd_pool` + `chunk_store` +
/// `chunk_cache` wired AND a chunked-OCI demo image. The test detects a
/// materialize-to-file fallback via the Phase A `GetCowState` diagnostic
/// (null state == not chunk-tracked) and skips the applied/idle assertions
/// with a `::warning::`. The endpoint-wired assertions (pre-write idle +
/// sandbox-bound success) still fire on every environment.
///
/// Test path:
/// 1. Cold-create a session against the demo image.
/// 2. **Always**: `flush_now` pre-write → assert `outcome=idle`.
/// 3. Probe `cow-state`. If null → `::warning::` + return.
/// 4. dd + sync into the chunked-disk-backed rootfs.
/// 5. `flush_now` → best-effort (scheduler may have drained).
/// 6. End-state: poll until `disk_manifest_version` advances.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_flush_now_applies_then_short_circuits_on_no_dirty() {
    let driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    // Step 2: pre-write flush. Environment-independent — the endpoint should
    // always return idle when nothing's dirty. A successful RPC + idle here
    // proves the RPC is mounted, the session-lookup works, and
    // `flush_sandbox` returns None cleanly.
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
    // chunked-disk path. `cow-state` is Some(...) only when the sandbox is
    // NBD-tracked. Where NBD is known-available a null here is a real
    // regression and we FAIL; elsewhere we skip with a loud warning.
    let cow = driver.cow_state(sid).await;
    let Some(cow) = cow else {
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
             Pre-write flush_now=idle assertion verified the endpoint wiring, but \
             the chunked-disk write → flush → publish round-trip can't be exercised \
             here."
        );
        driver.delete(sid).await;
        return;
    };

    // Step 4: capture the baseline disk_manifest_version BEFORE dd. The
    // FlushScheduler may already have ticked between create-session and now;
    // whatever version it left behind is what we measure forward from.
    let baseline_version = cow.disk_manifest_version;

    // Step 5: write enough dirty bytes to materialise at least one full
    // 16 MiB chunk in the chunked-disk dirty buffer. `/dev/zero` so writes
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
    let sync = driver.exec(sid, "sync").await;
    assert_eq!(sync.exit_status, Some(0), "sync should succeed");

    // Step 6: force the flush via the admin trigger — best-effort (the
    // FlushScheduler's 30s tick may have already drained the buffer).
    let _ = driver.flush_now(sid).await;

    // Step 7: end-state assertion. Poll cow-state until the disk manifest
    // version advances past `baseline_version`.
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
/// - `cow-state` on the resumed session returned null.
/// - A second eviction-snapshot errored with `non-canonical jail layout`.
///
/// Post-commit-5: resume rebuilds chunked-disk tracking; cow-state populates
/// for the resumed sandbox; flush-now against it drains chunks like a fresh
/// cold-create.
///
/// **Environment dependence**: same cow-state-probe skip pattern as
/// `e2e_flush_now_applies_then_short_circuits_on_no_dirty`.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_rejoins_chunked_disk_tracking() {
    let driver = Driver::from_env().await;
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
             can't exercise the NBD path. Snapshot+resume RPC wiring will \
             still be exercised below; cow-state-post-resume + \
             flush-now-post-resume assertions skipped."
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

    // Best-effort flush via the admin trigger (the snapshot below re-flushes
    // via its own backend.flush() regardless, so the chunks ARE durable in
    // BlobStorage by the time we evict).
    let _ = driver.flush_now(sid).await;

    // Snapshot → evict-local → resume. Equivalent to the idle-eviction →
    // resume cycle prod exercises, minus the 30-second idle wait.
    driver.snapshot(sid).await;
    driver.evict_local(sid).await;
    driver.resume(sid).await;

    // **THE REGRESSION CHECK**: post-resume cow-state must be Some(...).
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

    let _ = driver.flush_now(sid).await;

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

/// Lifecycle e2e: idle→active resume preserves BOTH disk and memory, proven
/// the only way a user would see it — write through the API, run the
/// snapshot→evict→resume cycle, read back through the API.
///
/// Pure black-box: it never inspects cow-state or manifest internals. Two
/// guarantees:
///   - **Disk data survives byte-identical.** A UUID sentinel written to
///     `/var/sentinel.txt` is `cat`'d back after resume and must match.
///   - **The kernel was memory-restored, not rebooted.**
///     `/proc/sys/kernel/random/boot_id` is minted once per kernel boot and
///     lives only in kernel memory — unchanged across a warm memory-restore,
///     regenerated on a cold reboot.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_resume_preserves_disk_and_memory() {
    let driver = Driver::from_env().await;
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

/// Lifecycle e2e: active→idle eviction, then resume with data intact — all
/// through the public API.
///
/// **gRPC PORT DIVERGENCE (ADR 0051):** the old HTTP harness fired the
/// `POST /admin/sessions/:id/evict-idle` admin trigger, which ran the
/// `idle_evictor::evict_idle_session` primitive (the SAME one the host
/// idle-detector and coord backstop scanner drive on a timeout). ADR 0051
/// DELETED that HTTP route and added NO gRPC equivalent (verified absent
/// from session.proto / fleet.proto). To preserve coverage of the
/// observable Active→Idle→resume-with-data-intact contract, this test now
/// drives the transition with `Snapshot` + `EvictLocal` (the manual
/// lifecycle path), exactly like `e2e_resume_preserves_disk_and_memory`. The
/// `evict_idle_session` *pipeline* itself (pause → flush → snapshot →
/// destroy as one op) is still unit-covered in `idle_evictor.rs`; what this
/// e2e now pins is the wire-level lifecycle + the byte-identical disk
/// survival across an Active→Idle→Active cycle over the gRPC surface. If a
/// dedicated idle-evict app RPC is ever added, restore the single-call
/// trigger here.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_idle_evict_then_resume_preserves_data() {
    let driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid = driver.create_session_none_harness(&image).await;

    let sentinel = uuid::Uuid::new_v4().to_string();
    let write = driver
        .exec(
            sid,
            &format!("printf '%s' {sentinel} > /var/idle-sentinel.txt && sync"),
        )
        .await;
    assert_eq!(
        write.exit_status,
        Some(0),
        "sentinel write should succeed; stderr=<{}>",
        write.stderr,
    );

    // Drive Active→Idle. Snapshot captures the running VM, EvictLocal drops
    // the local sandbox and transitions the session to Idle — the observable
    // end-state the deleted evict-idle trigger also produced.
    driver.snapshot(sid).await;
    driver.evict_local(sid).await;
    assert!(
        driver
            .wait_for_status(sid, "idle", Duration::from_secs(30))
            .await,
        "session should be Idle after snapshot + evict-local; got {}",
        driver.session_status(sid).await,
    );

    // Resumable, and the data written before eviction is intact.
    driver.resume(sid).await;
    assert!(
        driver
            .wait_for_status(sid, "active", Duration::from_secs(30))
            .await,
        "session should be Active after resume",
    );
    let readback = driver.exec(sid, "cat /var/idle-sentinel.txt").await;
    assert_eq!(
        readback.exit_status,
        Some(0),
        "sentinel readback should succeed; stderr=<{}>",
        readback.stderr,
    );
    assert_eq!(
        readback.stdout.trim(),
        sentinel,
        "disk data lost across idle-evict→resume",
    );

    driver.delete(sid).await;
}

/// ADR 0016 Phase C commit 6a — load-bearing regression test for the M5
/// failure class (chunk-GC silent-deleting a live image's chunks).
///
/// Flow:
/// 1. Create a none-harness session on the demo image (chunked-disk in CI).
/// 2. `ChunkGc { dry_run: true }` — assert `pin_set_size > 0`.
/// 3. `ChunkGc { dry_run: false, grace_secs: 0 }` — full sweep; MUST NOT
///    delete the demo image's chunks.
/// 4. Re-exec on the original session — proves the sweep didn't break its
///    data path.
/// 5. Create a SECOND session on the same image — strongest regression
///    catch; materializes the rootfs anew from BlobStorage.
/// 6. Second `ChunkGc { dry_run: false, grace_secs: 0 }` — idempotent on a
///    clean stack.
///
/// **gRPC mapping (ADR 0051):** the old paired `/api/admin/chunk-gc/dry-run`
/// + `/api/admin/chunk-gc/sweep` routes are folded into a single
/// `FleetService::ChunkGc` RPC discriminated by the `dry_run` flag (proto3
/// footgun: `dry_run: false` = LIVE sweep with deletions). The
/// `GET /api/admin/chunk-gc/candidates` route was INTENTIONALLY DROPPED by
/// ADR 0051 (no web caller; see fleet.proto's "INTENTIONAL DROPS" note), so
/// the old step-6 `/candidates` round-trip is removed — its response-shape
/// sanity check no longer has a surface to hit. The pin-set + sweep coverage
/// (the load-bearing M5 regression catch) is fully preserved.
///
/// Environment dependence: this test does NOT skip on missing NBD. The
/// pin-set + sweep paths run against PG + BlobStorage only.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_chunk_gc_sweep_does_not_delete_live_image_chunks() {
    let driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let sid_1 = driver.create_session_none_harness(&image).await;

    // Step 2: dry-run baseline. The pin set MUST cover the demo image's
    // chunks (enabled_images source). If this is 0, the regression has
    // already happened by construction.
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

    let pin_set_size_before = dry.pin_set_size;

    // Step 3: full sweep with grace=0. The load-bearing call.
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

    // Step 4: original session's data path still works.
    let ls = driver.exec(sid_1, "ls /").await;
    assert_eq!(
        ls.exit_status,
        Some(0),
        "post-sweep exec on original session failed — chunks may have \
         been deleted under it. stderr=<{}>",
        ls.stderr,
    );

    // Step 5: STRONGEST regression catch — a fresh session-create on the same
    // image materializes the rootfs anew from BlobStorage.
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

    // Step 6: idempotency on a clean stack. (The old `/candidates`
    // round-trip is dropped — ADR 0051 removed that route with no RPC.)
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

/// ADR 0018 Phase C admin-RPC shape coverage (now `FleetService::
/// EvacuateSession`).
///
/// The full multi-host alive-source evac requires two host-agents; the
/// single-host fixture exercises what's reachable:
///   - The RPC is mounted under bearer auth.
///   - `NotFound` on a non-existent session id (the old HTTP 404 →
///     `ApiError::NotFound` → `Code::NotFound`).
///   - For an Active session: returns `status="evacuating"` (the old HTTP
///     202 Accepted with `status="evacuating"`). The `evac_resumer` scanner
///     drives the session to Active on a peer in ≤10s.
///
/// Pinned regressions:
///   - RPC registration in `grpc_app::fleet` (a wiring slip makes every call
///     Unimplemented instead of answering).
///   - Pre-flight checks in `admin::evacuate_session_core` (NotFound vs
///     FailedPrecondition vs Internal).
///   - The async hand-off: the core MUST return without synchronously running
///     the relocate.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_evac_admin_endpoint_shape() {
    let driver = Driver::from_env().await;
    let image = Driver::image_uri();

    // Case 1: NotFound on a session that doesn't exist (HTTP 404 ↔
    // Code::NotFound).
    let bogus = SessionId::new();
    let err = driver
        .fleet()
        .evacuate_session(app::EvacuateSessionRequest {
            session_id: bogus.to_string(),
            target_host: None,
        })
        .await
        .expect_err("evacuate on unknown session should fail NotFound");
    assert_eq!(
        err.code(),
        Code::NotFound,
        "evacuate on unknown session should be Code::NotFound; got {err:?}",
    );

    // Case 2: live session — async shape. The core must return after marking
    // the session Evacuating; the scanner picks it up from there.
    let sid = driver.create_session_none_harness(&image).await;
    let resp = driver
        .fleet()
        .evacuate_session(app::EvacuateSessionRequest {
            session_id: sid.to_string(),
            target_host: None,
        })
        .await
        .expect("EvacuateSession on live session")
        .into_inner();
    assert_eq!(
        resp.status, "evacuating",
        "EvacuateSession response status must be \"evacuating\"; got {resp:?}",
    );
    assert_eq!(
        resp.session_id,
        sid.to_string(),
        "EvacuateSession response must echo session_id; got {resp:?}",
    );

    driver.delete(sid).await;
}

/// Full-stack 2-host teleport: relocate a live session to a peer and prove
/// its disk data crosses the move. Driven entirely through the app surface.
///
/// **gRPC PORT DIVERGENCE (ADR 0051):** the old HTTP harness fired
/// `POST /admin/sessions/:id/teleport {target_host_id}` — relocate to a
/// CHOSEN host. ADR 0051 DELETED that HTTP route and added NO host-pinning
/// gRPC equivalent (verified absent). The closest primitive is
/// `FleetService::EvacuateSession`, whose `target_host` field is explicitly
/// IGNORED by the handler — it relocates to ANY non-source peer via the
/// standard scheduler policy. In the two-host teleport stack there is
/// exactly ONE non-source peer, so "evacuate to any peer" is observationally
/// identical to "teleport to the one other host": `host_id == dest` still
/// holds, and every assertion (left the source, landed on the peer, disk
/// byte-identical) is preserved. If a host-pinned relocate RPC is ever added,
/// switch back to passing `dest` explicitly.
///
/// Requires the two-host stack (`ENGRAM_INTEG_TWO_HOSTS=1`, which the CI lane
/// sets alongside `ENGRAM_EXPECT_TWO_HOSTS=1`). On a single-host environment
/// it skips with a `::warning::` — unless `ENGRAM_EXPECT_TWO_HOSTS` is set,
/// where <2 hosts is a hard failure.
///
/// Flow: create on host A → write a UUID sentinel → evacuate → wait for
/// Active → assert `host_id` is now the peer (the sole non-source host) →
/// `cat` the sentinel back and assert byte-identical.
///
/// The test NAME is preserved verbatim — ci.yml's `nextest_filter` references
/// `e2e_two_host_teleport_preserves_sentinel`.
#[tokio::test]
#[ignore = "requires ENGRAM_E2E_GRPC_ADDR + a baked demo image + a 2-host stack; runs in ci.yml's test-e2e-stack lane"]
async fn e2e_two_host_teleport_preserves_sentinel() {
    let driver = Driver::from_env().await;
    let image = Driver::image_uri();

    let hosts = driver.list_host_ids().await;
    if hosts.len() < 2 {
        assert!(
            !two_hosts_required(),
            "ENGRAM_EXPECT_TWO_HOSTS is set but only {} host(s) registered — the \
             two-host stack didn't come up (check ENGRAM_INTEG_TWO_HOSTS + the \
             tilt-up-ci.sh >=2 host wait + NBD device split).",
            hosts.len(),
        );
        eprintln!(
            "::warning title=Teleport e2e skipped::only {} host registered; \
             teleport needs >=2. Set ENGRAM_INTEG_TWO_HOSTS=1 on the lane to \
             exercise this.",
            hosts.len(),
        );
        return;
    }

    let sid = driver.create_session_none_harness(&image).await;
    let src = driver
        .session_host_id(sid)
        .await
        .expect("Active session must have a bound host_id");

    let sentinel = uuid::Uuid::new_v4().to_string();
    let write = driver
        .exec(
            sid,
            &format!("printf '%s' {sentinel} > /var/tele-sentinel.txt && sync"),
        )
        .await;
    assert_eq!(
        write.exit_status,
        Some(0),
        "sentinel write should succeed; stderr=<{}>",
        write.stderr,
    );

    // The sole non-source peer — the host EvacuateSession will relocate to.
    let dest = hosts
        .iter()
        .find(|h| **h != src)
        .expect("a peer host distinct from the source");

    // Evacuate. `target_host` is ignored by the handler (it picks any
    // non-source peer); with exactly two hosts that peer is `dest`.
    driver
        .fleet()
        .evacuate_session(app::EvacuateSessionRequest {
            session_id: sid.to_string(),
            target_host: Some(dest.clone()),
        })
        .await
        .expect("EvacuateSession (teleport)");

    // The evac_resumer drives Evacuating → Created → Active on the peer.
    // Generous deadline: first restore on the peer may cold-fetch chunks.
    assert!(
        driver
            .wait_for_status(sid, "active", Duration::from_secs(180))
            .await,
        "session should be Active on the destination host after teleport; \
         last status={}",
        driver.session_status(sid).await,
    );

    // Landed on the peer, not the source.
    let after = driver
        .session_host_id(sid)
        .await
        .expect("resumed session must have a bound host_id");
    assert_ne!(after, src, "session must leave the source host");
    assert_eq!(
        after, *dest,
        "session must land on the sole non-source peer (== the teleport target)",
    );

    // Disk data crossed the move byte-identical.
    let readback = driver.exec(sid, "cat /var/tele-sentinel.txt").await;
    assert_eq!(
        readback.exit_status,
        Some(0),
        "sentinel readback on the destination host should succeed; stderr=<{}>",
        readback.stderr,
    );
    assert_eq!(
        readback.stdout.trim(),
        sentinel,
        "disk data lost across the host teleport",
    );

    driver.delete(sid).await;
}
