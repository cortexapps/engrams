//! Transport-agnostic exec core for the app-gRPC `SessionService::Exec`
//! stream and ADR 0103's deterministic co-simulator.
//!
//! The core resolves the session environment and sandbox, drives the shared
//! `HostClient::exec_stream` path, persists genuine lifecycle/output events,
//! and preserves end-without-Exit as retryable transport loss for callers
//! that can re-attach to the durable exec ticket.

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::ExecLifecycleEventKind;
use engram_core::types::{ExecEvent, ExecRequest as SandboxExecRequest, ExecRusage};
use engram_core::SessionId;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

#[derive(Deserialize)]
pub struct ExecRequest {
    /// Either a single shell command (passed to `sh -c`) or an explicit
    /// argv vector. Exactly one of `command` / `argv` must be set.
    pub command: Option<String>,
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    pub timeout_secs: Option<u64>,
    pub exec_id: Option<String>,
    pub stdout_offset: Option<u64>,
    pub stderr_offset: Option<u64>,
    pub wake: Option<bool>,
}

/// Resolve the image's launch env + default workdir for `id` so exec'd
/// processes inherit the same environment + cwd the harness would get
/// (manifest `[env]` + resolved `[secrets]` + per-request secret
/// overrides, via [`crate::api::sessions::resolve_session_env`]).
///
/// This is the fix for agentless `dev_vm` sessions, whose *only* entry
/// point is exec: agentd inherits a bare boot env (default `PATH`, no
/// manifest env, no secrets) and a `/` cwd, and the harness-spawn path
/// — the one place that injected the manifest env — is skipped entirely
/// in dev-VM mode. Without this, `engram exec` in such a session sees
/// none of the image's environment.
///
/// Best-effort: a session/manifest load failure degrades to
/// request-only env (the pre-injection behaviour) and never blocks
/// exec — the subsequent `ensure_active` / registry lookup surfaces any
/// real "no such session" error.
async fn session_exec_env(
    state: &SharedState,
    id: SessionId,
) -> (HashMap<String, String>, Option<String>) {
    // agentd holds the durable session env (image `[env]` + secrets +
    // session id) from the bind and applies it to every exec, so we don't
    // re-resolve it here (that would just duplicate what agentd already
    // has). The exec wire env carries only the per-request forge broker
    // token — a short-lived credential deliberately kept out of the cached
    // session env so it's minted fresh and survives a coord restart — plus
    // the manifest's default workdir.
    let session = match state.services.meta.get_session(id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "exec: session load failed; forge/workdir injection skipped",
            );
            return (HashMap::new(), None);
        }
    };
    let bundle = match crate::api::sessions::resume_manifest_bundle(state, &session).await {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "exec: manifest bundle load failed; forge/workdir injection skipped",
            );
            None
        }
    };
    let mut env = HashMap::new();
    if bundle.is_some() {
        crate::api::sessions::inject_forge_env(state, id, &mut env).await;
    }
    // ADR 0026: upload token is not git-gated and doesn't need the
    // manifest bundle — inject it unconditionally so `engram-share`
    // works from `/exec` even when the bundle load above failed.
    crate::api::sessions::inject_upload_env(state, id, &mut env).await;
    (env, bundle.and_then(|b| b.config.workdir))
}

/// Helper: validate the request shape and produce an argv + sandbox
/// ExecRequest. Centralised so the sync and streaming endpoints stay
/// consistent.
///
/// `base_env` is the per-request exec additions (the forge broker token;
/// see [`session_exec_env`]) — the durable image env + secrets are held by
/// agentd and applied *underneath* this. The request's own `env` layers on
/// top so a caller can override a default. `ENGRAM_SESSION_ID` is injected
/// last so neither can clobber it (agentd also carries it in session_env;
/// setting it here keeps exec self-describing). `default_workdir` (the
/// manifest `workdir`) applies only when the request doesn't carry its own.
fn build_exec(
    req: ExecRequest,
    session: SessionId,
    base_env: HashMap<String, String>,
    default_workdir: Option<String>,
) -> Result<(Vec<String>, SandboxExecRequest), ApiError> {
    let argv = match (req.command.as_ref(), req.argv.as_ref()) {
        (Some(c), None) => vec!["sh".into(), "-c".into(), c.clone()],
        (None, Some(a)) if !a.is_empty() => a.clone(),
        _ => {
            return Err(ApiError::BadRequest(
                "exactly one of `command` or non-empty `argv` is required".into(),
            ));
        }
    };
    let mut env = base_env;
    env.extend(req.env);
    env.insert("ENGRAM_SESSION_ID".into(), session.to_string());
    let sandbox_req = SandboxExecRequest {
        command: argv.clone(),
        stdin: None,
        env,
        workdir: req.workdir.or(default_workdir),
        timeout: req.timeout_secs.map(Duration::from_secs),
        exec_id: req.exec_id,
        stdout_offset: req.stdout_offset,
        stderr_offset: req.stderr_offset,
        wake: req.wake,
    };
    Ok((argv, sandbox_req))
}

// ----------------------------------------------------------------
// ADR 0051: transport-agnostic streaming exec core for the app-gRPC
// `Exec` RPC. Same backend path + bus-persistence as the SSE handler
// (`exec_stream`); the gRPC transport prepends the `started` frame and
// maps these events to proto. Persistence-failure posture matches the
// SSE handler (log + continue the live tail).
// ----------------------------------------------------------------

/// One frame of the streaming-exec body (after the `started` frame the
/// gRPC handler prepends). The terminal `Exit` carries the same
/// coordinator-side wall-time rusage the SSE/sync paths compute. A backend
/// stream that ends without a real `Exit` yields a retryable error instead.
#[derive(Debug)]
pub enum ExecStreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit {
        exit_status: Option<i32>,
        rusage: ExecRusage,
    },
}

/// Transport-agnostic `Exec` core. Resolves the exec env, auto-resumes the
/// session, kicks off the backend exec stream, and returns `(exec_id, body)`
/// where `body` yields stdout/stderr chunks then either a terminal `Exit` or
/// a retryable error when the backend stream ends first — persisting each
/// genuine event to the session bus exactly as the SSE handler does.
///
/// Public so ADR 0103's co-simulator can exercise the real coordinator
/// persistence path over the real host/guest exec protocol boundary.
pub async fn exec_stream_core(
    state: &SharedState,
    id: SessionId,
    req: ExecRequest,
) -> Result<
    (
        String,
        std::pin::Pin<Box<dyn Stream<Item = Result<ExecStreamEvent, ApiError>> + Send>>,
    ),
    ApiError,
> {
    let (base_env, default_workdir) = session_exec_env(state, id).await;
    let wake = req.wake.unwrap_or(true);
    let caller_supplied_exec_id = req.exec_id.is_some();
    let zero_offsets = req.stdout_offset.unwrap_or(0) == 0 && req.stderr_offset.unwrap_or(0) == 0;
    let (argv, mut sandbox_req) = build_exec(req, id, base_env, default_workdir)?;
    let requested_exec_id = sandbox_req
        .exec_id
        .clone()
        .unwrap_or_else(|| format!("exec:{}", state.services.entropy.uuid().simple()));
    sandbox_req.exec_id = Some(requested_exec_id.clone());

    if wake {
        crate::api::snapshot::ensure_active(state, id).await?;
    }
    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox — create a new session or resume from snapshot".into(),
        )
    })?;

    let backend_stream = state
        .services
        .host
        .exec_stream(sandbox_id, sandbox_req)
        .await?;
    let exec_id = backend_stream.exec_id.clone();
    if exec_id != requested_exec_id {
        return Err(ApiError::Conflict(format!(
            "durable exec identity changed across host boundary: requested {requested_exec_id}, got {exec_id}"
        )));
    }

    let emit_exec_started = if !zero_offsets {
        false
    } else if caller_supplied_exec_id {
        state
            .services
            .meta
            .session_exec_event_logged_at(id, &exec_id, ExecLifecycleEventKind::Started)
            .await?
            .is_none()
    } else {
        // A coordinator-minted id cannot be a re-attach: the caller did not
        // know the ticket before this request.
        true
    };
    if emit_exec_started {
        // Non-zero offset re-attaches skip lifecycle emission by construction.
        // A legitimate zero-byte re-attach still arrives at `(0, 0)`, so the
        // durable event-log predicate above enforces the residual
        // consumer-idempotency expectation at the source, keyed by exec_id.
        state
            .emit(
                id,
                SessionEvent::ExecStarted {
                    exec_id: exec_id.clone(),
                    command: argv.clone(),
                    at: state.services.clock.now_utc(),
                },
            )
            .await?;
    }

    let state_for_stream = state.clone();
    let exec_id_for_stream = exec_id.clone();
    let started_at = state.services.clock.now_mono();
    let body = async_stream::stream! {
        let mut events = backend_stream.events;
        let mut exit_status = None;
        let mut saw_exit = false;
        while let Some(ev) = events.next().await {
            match ev {
                ExecEvent::Stdout(bytes) => {
                    let chunk = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = state_for_stream
                        .emit(id, SessionEvent::Stdout {
                            exec_id: exec_id_for_stream.clone(),
                            chunk,
                        })
                        .await
                        .map_err(|e| tracing::warn!(error = %e, "stdout event persistence failed; live tail continues"));
                    yield Ok(ExecStreamEvent::Stdout(bytes.to_vec()));
                }
                ExecEvent::Stderr(bytes) => {
                    let chunk = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = state_for_stream
                        .emit(id, SessionEvent::Stderr {
                            exec_id: exec_id_for_stream.clone(),
                            chunk,
                        })
                        .await
                        .map_err(|e| tracing::warn!(error = %e, "stderr event persistence failed; live tail continues"));
                    yield Ok(ExecStreamEvent::Stderr(bytes.to_vec()));
                }
                ExecEvent::Exit(code) => {
                    exit_status = code;
                    saw_exit = true;
                    break;
                }
            }
        }
        if !saw_exit {
            yield Err(ApiError::Unavailable(format!(
                "exec {exec_id_for_stream} backend stream ended without an Exit frame; \
                 its result may still be recoverable by re-attaching with the same exec_id"
            )));
            return;
        }
        // wall_ms spans from the LOGGED exec_started to this Exit. On the
        // ADR's happy path the Exit is delivered by a later re-attach, so
        // the delivering segment is only a fraction of the exec's real
        // runtime; the attach-segment measure is kept as the fallback for
        // stores without the event log (or a failed lookup).
        let logged_started_at = state_for_stream
            .services
            .meta
            .session_exec_event_logged_at(
                id,
                &exec_id_for_stream,
                ExecLifecycleEventKind::Started,
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "exec_started lookup failed; wall_ms falls back to the attach segment",
                );
                None
            });
        let wall_ms = match logged_started_at {
            Some(started) => (state_for_stream.services.clock.now_utc() - started)
                .num_milliseconds()
                .max(0) as u64,
            None => state_for_stream
                .services
                .clock
                .now_mono()
                .saturating_sub(started_at)
                .as_millis() as u64,
        };
        let rusage = ExecRusage {
            wall_ms,
            ..ExecRusage::default()
        };
        // A journal replay of an already-complete exec reaches a real Exit
        // on EVERY attach, so completion is deduplicated by exec_id against
        // the durable log — same residual concurrent-attach race as the
        // ExecStarted dedup above; a failed lookup degrades to at-least-once.
        let already_completed = state_for_stream
            .services
            .meta
            .session_exec_event_logged_at(
                id,
                &exec_id_for_stream,
                ExecLifecycleEventKind::Completed,
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "exec_completed dedup lookup failed; emitting (at-least-once)",
                );
                None
            })
            .is_some();
        if !already_completed {
            let _ = state_for_stream
                .emit(id, SessionEvent::ExecCompleted {
                    exec_id: exec_id_for_stream.clone(),
                    exit_status,
                    rusage,
                    at: state_for_stream.services.clock.now_utc(),
                })
                .await
                .map_err(|e| tracing::warn!(error = %e, "exec_completed event persistence failed"));
        }
        yield Ok(ExecStreamEvent::Exit { exit_status, rusage });
    };

    Ok((exec_id, Box::pin(body)))
}

pub(crate) async fn cancel_exec_core(
    state: &SharedState,
    id: SessionId,
    exec_id: String,
) -> Result<(), ApiError> {
    if exec_id.is_empty() {
        return Err(ApiError::BadRequest("exec_id must not be empty".into()));
    }
    crate::api::snapshot::ensure_active(state, id).await?;
    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox — create a new session or resume from snapshot".into(),
        )
    })?;
    state.services.host.cancel_exec(sandbox_id, exec_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use engram_core::traits::{HostClient, MetadataStore, SessionFence};
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::sandbox::{
        AgentSpec, ExecRequest as HostExecRequest, ExecStream, SandboxProbe, SandboxSpec,
    };
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::types::{SessionSpec, SessionState};
    use engram_core::{SandboxError, SandboxId};
    use engram_sim::{ManualClock, MemBlobStorage, SimEntropy, SimMetadataStore};

    use super::*;

    struct StubExecHost {
        streams: parking_lot::Mutex<VecDeque<Vec<ExecEvent>>>,
    }

    impl StubExecHost {
        fn new(streams: impl IntoIterator<Item = Vec<ExecEvent>>) -> Self {
            Self {
                streams: parking_lot::Mutex::new(streams.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl HostClient for StubExecHost {
        async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            unreachable!("exec-core tests never create sandboxes")
        }

        async fn destroy(&self, _id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
            unreachable!("exec-core tests never destroy sandboxes")
        }

        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            unreachable!("exec-core tests never list sandboxes")
        }

        async fn probe_sandbox(&self, _id: SandboxId) -> Result<SandboxProbe, SandboxError> {
            unreachable!("exec-core tests never probe sandboxes")
        }

        async fn exec_stream(
            &self,
            id: SandboxId,
            cmd: HostExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            let events = self
                .streams
                .lock()
                .pop_front()
                .expect("one scripted host stream per exec call");
            Ok(ExecStream {
                sandbox_id: id,
                exec_id: cmd.exec_id.expect("coordinator supplies exec_id"),
                events: Box::pin(futures::stream::iter(events)),
            })
        }

        async fn snapshot(
            &self,
            _id: SandboxId,
            _fence: SessionFence,
        ) -> Result<SnapshotMetadata, SandboxError> {
            unreachable!("exec-core tests never snapshot sandboxes")
        }

        async fn restore(
            &self,
            _metadata: SnapshotMetadata,
            _fence: SessionFence,
        ) -> Result<SandboxId, SandboxError> {
            unreachable!("exec-core tests never restore sandboxes")
        }

        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: AgentSpec,
            _policy: SessionEgressPolicy,
            _fence: SessionFence,
        ) -> Result<(), SandboxError> {
            unreachable!("exec-core tests never start agents")
        }

        async fn apply_egress_policy(
            &self,
            _policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unreachable!("exec-core tests never apply egress policy")
        }

        async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
            None
        }

        async fn bind_session(
            &self,
            _session_id: SessionId,
            _sandbox_id: SandboxId,
            _binding_epoch: u64,
        ) {
        }

        async fn unbind_session(&self, _session_id: SessionId) {}

        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _prompt_id: String,
            _text: String,
        ) -> Result<(), SandboxError> {
            unreachable!("exec-core tests never send prompts")
        }
    }

    fn exec_test_state(
        streams: impl IntoIterator<Item = Vec<ExecEvent>>,
    ) -> (SharedState, Arc<SimMetadataStore>, Arc<ManualClock>) {
        let clock = ManualClock::new();
        let entropy = Arc::new(SimEntropy::seeded(0xE103));
        let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
        let blob = Arc::new(MemBlobStorage::new());
        let services = crate::Services {
            meta: meta.clone(),
            host: Arc::new(StubExecHost::new(streams)),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: blob.clone(),
            chunk_store: engram_chunk_store::ChunkStore::new(blob),
            materialize_dir: None,
            clock: clock.clone(),
            entropy,
        };
        (
            Arc::new(crate::AppState::new(
                crate::CoordinatorConfig::default(),
                services,
            )),
            meta,
            clock,
        )
    }

    async fn stage_exec_session(meta: &Arc<SimMetadataStore>) -> (SessionId, SandboxId) {
        let sandbox_id = SandboxId::new();
        let session_id = meta
            .create_session(SessionSpec {
                image: "test.invalid/exec:latest".into(),
                mode: Default::default(),
            })
            .await
            .expect("create exec test session");
        meta.transition_session_created(session_id, sandbox_id)
            .await
            .expect("bind exec test sandbox");
        meta.transition_session(session_id, SessionState::Active)
            .await
            .expect("activate exec test session");
        (session_id, sandbox_id)
    }

    fn durable_req(
        exec_id: &str,
        stdout_offset: Option<u64>,
        stderr_offset: Option<u64>,
    ) -> ExecRequest {
        ExecRequest {
            command: Some("true".into()),
            argv: None,
            env: HashMap::new(),
            workdir: None,
            timeout_secs: None,
            exec_id: Some(exec_id.into()),
            stdout_offset,
            stderr_offset,
            wake: Some(false),
        }
    }

    fn event_kinds(meta: &SimMetadataStore, session_id: SessionId) -> Vec<String> {
        meta.with_db(|db| {
            db.session_events
                .get(&session_id)
                .into_iter()
                .flatten()
                .map(|event| event.kind.clone())
                .collect()
        })
    }

    fn exec_started_ids(meta: &SimMetadataStore, session_id: SessionId) -> Vec<String> {
        meta.with_db(|db| {
            db.session_events
                .get(&session_id)
                .into_iter()
                .flatten()
                .filter(|event| event.kind == "exec_started")
                .filter_map(|event| {
                    event
                        .payload
                        .get("exec_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
    }

    fn req(env: &[(&str, &str)], workdir: Option<&str>) -> ExecRequest {
        ExecRequest {
            command: Some("true".into()),
            argv: None,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            workdir: workdir.map(str::to_string),
            timeout_secs: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        }
    }

    fn base(env: &[(&str, &str)]) -> HashMap<String, String> {
        env.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn base_env_is_the_base_request_env_overrides_session_id_wins() {
        let session = SessionId::new();
        let (_argv, sandbox_req) = build_exec(
            req(&[("B", "req"), ("C", "req")], None),
            session,
            base(&[("A", "img"), ("B", "img")]),
            None,
        )
        .unwrap();
        // base_env (per-request additions — the forge token in prod) is the
        // base; request env overrides on collision (B); both base-only (A)
        // and request-only (C) survive. The durable image env lives a layer
        // below this, applied by the backend/agentd, not here.
        assert_eq!(sandbox_req.env.get("A").map(String::as_str), Some("img"));
        assert_eq!(sandbox_req.env.get("B").map(String::as_str), Some("req"));
        assert_eq!(sandbox_req.env.get("C").map(String::as_str), Some("req"));
        // ENGRAM_SESSION_ID is injected last and can't be clobbered.
        assert_eq!(
            sandbox_req.env.get("ENGRAM_SESSION_ID").map(String::as_str),
            Some(session.to_string().as_str()),
        );
    }

    #[test]
    fn request_cannot_clobber_session_id() {
        let session = SessionId::new();
        let (_argv, sandbox_req) = build_exec(
            req(&[("ENGRAM_SESSION_ID", "evil")], None),
            session,
            base(&[]),
            None,
        )
        .unwrap();
        assert_eq!(
            sandbox_req.env.get("ENGRAM_SESSION_ID").map(String::as_str),
            Some(session.to_string().as_str()),
        );
    }

    #[test]
    fn durable_attach_fields_reach_the_host_request_unchanged() {
        let session = SessionId::new();
        let mut request = req(&[], None);
        request.exec_id = Some("exec:caller-ticket".into());
        request.stdout_offset = Some(123);
        request.stderr_offset = Some(456);
        request.wake = Some(false);
        let (_argv, sandbox_req) = build_exec(request, session, base(&[]), None).unwrap();
        assert_eq!(sandbox_req.exec_id.as_deref(), Some("exec:caller-ticket"));
        assert_eq!(sandbox_req.stdout_offset, Some(123));
        assert_eq!(sandbox_req.stderr_offset, Some(456));
        assert_eq!(sandbox_req.wake, Some(false));
    }

    #[test]
    fn workdir_defaults_to_manifest_but_request_wins() {
        let session = SessionId::new();
        // No request workdir → the manifest default applies.
        let (_a, with_default) = build_exec(
            req(&[], None),
            session,
            base(&[]),
            Some("/workspace".into()),
        )
        .unwrap();
        assert_eq!(with_default.workdir.as_deref(), Some("/workspace"));
        // Request workdir wins over the manifest default.
        let (_b, overridden) = build_exec(
            req(&[], Some("/tmp/here")),
            session,
            base(&[]),
            Some("/workspace".into()),
        )
        .unwrap();
        assert_eq!(overridden.workdir.as_deref(), Some("/tmp/here"));
        // Neither set → None (sandbox's own default cwd).
        let (_c, neither) = build_exec(req(&[], None), session, base(&[]), None).unwrap();
        assert_eq!(neither.workdir, None);
    }

    #[tokio::test]
    async fn exec_started_is_persisted_only_for_zero_offset_attaches() {
        for (stdout_offset, stderr_offset, expected_started) in [
            (None, None, 1),
            (Some(0), Some(0), 1),
            (Some(6), Some(0), 0),
        ] {
            let (state, meta, _clock) = exec_test_state([vec![ExecEvent::Exit(Some(0))]]);
            let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
            let (_exec_id, mut body) = exec_stream_core(
                &state,
                session_id,
                durable_req("exec:lifecycle", stdout_offset, stderr_offset),
            )
            .await
            .expect("start exec stream");
            while let Some(item) = body.next().await {
                item.expect("genuine exit stream is successful");
            }

            let started = event_kinds(&meta, session_id)
                .into_iter()
                .filter(|kind| kind == "exec_started")
                .count();
            assert_eq!(
                started, expected_started,
                "stdout_offset={stdout_offset:?}, stderr_offset={stderr_offset:?}"
            );
        }
    }

    #[tokio::test]
    async fn zero_byte_reattach_deduplicates_exec_started_by_exec_id() {
        let (state, meta, _clock) = exec_test_state([
            vec![ExecEvent::Exit(Some(0))],
            vec![ExecEvent::Exit(Some(0))],
            vec![ExecEvent::Exit(Some(0))],
        ]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;

        for exec_id in ["exec:zero-byte", "exec:zero-byte", "exec:different"] {
            let (_exec_id, mut body) =
                exec_stream_core(&state, session_id, durable_req(exec_id, Some(0), Some(0)))
                    .await
                    .expect("start exec stream");
            while let Some(item) = body.next().await {
                item.expect("genuine exit stream is successful");
            }
        }

        assert_eq!(
            exec_started_ids(&meta, session_id),
            vec!["exec:zero-byte", "exec:different"]
        );
    }

    #[tokio::test]
    async fn backend_end_without_exit_yields_retryable_error_and_no_completion() {
        let (state, meta, _clock) =
            exec_test_state([vec![ExecEvent::Stdout(Bytes::from_static(b"partial"))]]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let (_exec_id, mut body) =
            exec_stream_core(&state, session_id, durable_req("exec:severed", None, None))
                .await
                .expect("start exec stream");

        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Stdout(bytes))) if bytes == b"partial"
        ));
        match body.next().await {
            Some(Err(ApiError::Unavailable(message))) => {
                assert!(message.contains("recoverable"));
                assert!(message.contains("same exec_id"));
            }
            other => panic!("expected retryable unavailable error, got {other:?}"),
        }
        assert!(body.next().await.is_none());

        let kinds = event_kinds(&meta, session_id);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| kind.as_str() == "exec_completed")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn genuine_exit_persists_exactly_one_completion() {
        let (state, meta, _clock) = exec_test_state([vec![ExecEvent::Exit(Some(7))]]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let (_exec_id, mut body) = exec_stream_core(
            &state,
            session_id,
            durable_req("exec:completed", None, None),
        )
        .await
        .expect("start exec stream");

        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(7),
                ..
            }))
        ));
        assert!(body.next().await.is_none());
        let kinds = event_kinds(&meta, session_id);
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| kind.as_str() == "exec_completed")
                .count(),
            1
        );
    }

    /// The ADR's own happy path: a checkpoint severs the delivering attach,
    /// the caller re-attaches, and the journal's Exit lands on a LATER
    /// `exec_stream_core` call. `exec_completed` must land exactly once and
    /// `wall_ms` must span from the logged `exec_started`, not just the
    /// attach segment that happened to deliver the Exit.
    #[tokio::test]
    async fn completed_journal_replay_deduplicates_exec_completed_with_true_wall() {
        let (state, meta, clock) = exec_test_state([
            // Attach 1: severed after partial output, no Exit.
            vec![ExecEvent::Stdout(Bytes::from_static(b"partial"))],
            // Attach 2: the journal delivers the real Exit.
            vec![ExecEvent::Exit(Some(0))],
            // Attach 3: a later caller replays the already-complete journal.
            vec![ExecEvent::Exit(Some(0))],
        ]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let exec_id = "exec:true-wall";

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, None, None))
                .await
                .expect("start exec stream");
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Stdout(bytes))) if bytes == b"partial"
        ));
        assert!(matches!(
            body.next().await,
            Some(Err(ApiError::Unavailable(_)))
        ));
        assert!(body.next().await.is_none());

        // The command keeps running in the guest for a minute before the
        // caller's re-attach picks the result up.
        clock.advance(Duration::from_secs(60));
        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, Some(7), Some(0)))
                .await
                .expect("re-attach exec stream");
        match body.next().await {
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(0),
                rusage,
            })) => assert_eq!(
                rusage.wall_ms, 60_000,
                "wall_ms must span from the logged exec_started, not the delivering attach segment"
            ),
            other => panic!("expected the journal's Exit, got {other:?}"),
        }
        assert!(body.next().await.is_none());

        clock.advance(Duration::from_secs(10));
        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, Some(7), Some(0)))
                .await
                .expect("replay exec stream");
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(0),
                ..
            }))
        ));
        assert!(body.next().await.is_none());

        let completions = event_kinds(&meta, session_id)
            .into_iter()
            .filter(|kind| kind == "exec_completed")
            .count();
        assert_eq!(
            completions, 1,
            "a replay of an already-complete journal must not append another exec_completed"
        );
    }
}
