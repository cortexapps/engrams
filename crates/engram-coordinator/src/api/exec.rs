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
/// refusal yields a non-retryable error; a stream that ends without either
/// terminal yields a retryable error.
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
/// an error: non-retryable for `Refused`, retryable when the backend stream
/// ends first. Genuine events are persisted to the session bus exactly as
/// the SSE handler does.
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
    // Absolute positions of the first byte each stream will deliver on THIS
    // attach — the base for the persisted chunk rows' byte-range stamps.
    let stdout_attach_offset = req.stdout_offset.unwrap_or(0);
    let stderr_attach_offset = req.stderr_offset.unwrap_or(0);
    let zero_offsets = stdout_attach_offset == 0 && stderr_attach_offset == 0;
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

    // Whether THIS attach is eligible to record `ExecStarted`: a zero-offset
    // attach with no prior live Started for the ticket. Non-zero offsets are
    // re-attaches (never a start); a coordinator-minted id cannot be a
    // re-attach. The row is NOT emitted here — the terminal taxonomy says a
    // Refusal records no lifecycle rows, and a refusal is only observable
    // once the backend stream is polled. Emission is deferred to the first
    // NON-`Refused` frame below, so a refused attach records nothing (and
    // cannot poison the ticket's start-dedup for a later real run).
    let emit_exec_started = if !zero_offsets {
        false
    } else if caller_supplied_exec_id {
        state
            .services
            .meta
            .session_exec_event_at(id, &exec_id, ExecLifecycleEventKind::Started)
            .await?
            .is_none()
    } else {
        true
    };

    let state_for_stream = state.clone();
    let exec_id_for_stream = exec_id.clone();
    let started_at = state.services.clock.now_mono();
    // Captured at ATTACH time for the deferred `exec_started` stamp below:
    // a silent command's first coordinator-level frame is its Exit (agentd's
    // wire Started never reaches this layer), so stamping at first-frame
    // delivery would collapse wall_ms to ~0 for exactly the silent-clone
    // shape this ADR makes durable. Attach time is the closest host-side
    // proxy for spawn time; only the emission is deferred (for the
    // refusal-records-nothing rule), not the measurement.
    let attach_at_utc = state.services.clock.now_utc();
    // ADR 0103: output recording is observation-independent. Chunk rows are
    // stamped with their absolute RAW byte range, and this attach skips
    // persisting anything at or below the mark already recorded for the
    // ticket — attaching N times records the same rows as attaching once.
    // Bytes still STREAM to the caller regardless (it asked for them from
    // its own offsets); only the durable record is deduplicated. A failed
    // lookup degrades to persist-everything (at-least-once), same posture
    // as the lifecycle dedup.
    let stdout_high_water = state
        .services
        .meta
        .session_exec_output_high_water(id, &exec_id, engram_core::traits::ExecOutputStream::Stdout)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "stdout high-water lookup failed; recording at-least-once");
            0
        });
    let stderr_high_water = state
        .services
        .meta
        .session_exec_output_high_water(id, &exec_id, engram_core::traits::ExecOutputStream::Stderr)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "stderr high-water lookup failed; recording at-least-once");
            0
        });
    let mut stdout_pos = stdout_attach_offset;
    let mut stderr_pos = stderr_attach_offset;
    let started_command = argv.clone();
    let body = async_stream::stream! {
        let mut events = backend_stream.events;
        let mut exit_status = None;
        let mut saw_exit = false;
        let mut started_emitted = false;
        while let Some(ev) = events.next().await {
            // Deferred ExecStarted: emit once, on the first frame that is not
            // a Refusal. A Refused attach falls through to its arm below
            // having recorded no lifecycle row. Best-effort like the other
            // streaming emits (the module's log-and-continue posture).
            if emit_exec_started && !started_emitted && !matches!(ev, ExecEvent::Refused(_)) {
                started_emitted = true;
                let _ = state_for_stream
                    .emit(id, SessionEvent::ExecStarted {
                        exec_id: exec_id_for_stream.clone(),
                        command: started_command.clone(),
                        at: attach_at_utc,
                    })
                    .await
                    .map_err(|e| tracing::warn!(error = %e, "exec_started event persistence failed; live tail continues"));
            }
            match ev {
                ExecEvent::Stdout(bytes) => {
                    let bytes_start = stdout_pos;
                    let bytes_end = bytes_start + bytes.len() as u64;
                    stdout_pos = bytes_end;
                    if bytes_end > stdout_high_water {
                        // Trim the RAW bytes of a straddling chunk before the
                        // lossy decode so the recorded range stays exact.
                        let persist_from = bytes_start.max(stdout_high_water);
                        let raw = &bytes[(persist_from - bytes_start) as usize..];
                        let chunk = String::from_utf8_lossy(raw).into_owned();
                        let _ = state_for_stream
                            .emit(id, SessionEvent::Stdout {
                                exec_id: exec_id_for_stream.clone(),
                                chunk,
                                bytes_start: persist_from,
                                bytes_end,
                            })
                            .await
                            .map_err(|e| tracing::warn!(error = %e, "stdout event persistence failed; live tail continues"));
                    }
                    yield Ok(ExecStreamEvent::Stdout(bytes.to_vec()));
                }
                ExecEvent::Stderr(bytes) => {
                    let bytes_start = stderr_pos;
                    let bytes_end = bytes_start + bytes.len() as u64;
                    stderr_pos = bytes_end;
                    if bytes_end > stderr_high_water {
                        let persist_from = bytes_start.max(stderr_high_water);
                        let raw = &bytes[(persist_from - bytes_start) as usize..];
                        let chunk = String::from_utf8_lossy(raw).into_owned();
                        let _ = state_for_stream
                            .emit(id, SessionEvent::Stderr {
                                exec_id: exec_id_for_stream.clone(),
                                chunk,
                                bytes_start: persist_from,
                                bytes_end,
                            })
                            .await
                            .map_err(|e| tracing::warn!(error = %e, "stderr event persistence failed; live tail continues"));
                    }
                    yield Ok(ExecStreamEvent::Stderr(bytes.to_vec()));
                }
                ExecEvent::Exit(code) => {
                    exit_status = code;
                    saw_exit = true;
                    break;
                }
                ExecEvent::Refused(reason) => {
                    yield Err(ApiError::Conflict(format!("exec refused: {reason}")));
                    return;
                }
            }
        }
        if !saw_exit {
            yield Err(ApiError::Unavailable(format!(
                "exec {exec_id_for_stream} backend stream ended without an Exit or Refused frame; \
                 its result may still be recoverable by re-attaching with the same exec_id"
            )));
            return;
        }
        // wall_ms spans from the RECORDED exec_started stamp (the attach
        // time it carries — see `session_exec_event_at`) to this Exit. On
        // the ADR's happy path the Exit is delivered by a later re-attach,
        // so the delivering segment is only a fraction of the exec's real
        // runtime; the attach-segment measure is kept as the fallback for
        // stores without the event log (or a failed lookup).
        let recorded_started_at = state_for_stream
            .services
            .meta
            .session_exec_event_at(
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
        let wall_ms = match recorded_started_at {
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
            .session_exec_event_at(
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
            _mode: Option<String>,
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
        meta.transition_session(
            session_id,
            SessionState::Active,
            engram_core::types::BindingDisposition::Retain,
        )
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

    fn exec_completed_statuses(meta: &SimMetadataStore, session_id: SessionId) -> Vec<Option<i64>> {
        meta.with_db(|db| {
            db.session_events
                .get(&session_id)
                .into_iter()
                .flatten()
                .filter(|event| event.kind == "exec_completed")
                .map(|event| {
                    event
                        .payload
                        .get("exit_status")
                        .and_then(serde_json::Value::as_i64)
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

    #[tokio::test]
    async fn refusal_cannot_poison_later_genuine_completion_for_same_exec_id() {
        let (state, meta, _clock) = exec_test_state([
            vec![ExecEvent::Refused("first writer wins".into())],
            vec![ExecEvent::Exit(Some(0))],
        ]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let exec_id = "exec:refusal-poisoning";

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, None, None))
                .await
                .expect("start refused exec stream");
        match body.next().await {
            Some(Err(ApiError::Conflict(message))) => {
                assert!(message.starts_with("exec refused: "), "{message}");
                assert!(message.contains("first writer wins"), "{message}");
            }
            other => panic!("expected terminal refusal error, got {other:?}"),
        }
        assert!(body.next().await.is_none());
        assert!(
            exec_completed_statuses(&meta, session_id).is_empty(),
            "a refusal must never persist exec_completed"
        );
        // The taxonomy: a refusal records NO lifecycle rows. A stray
        // exec_started here would also poison the ticket's start-dedup, so
        // the later real run's start would be suppressed.
        assert!(
            exec_started_ids(&meta, session_id).is_empty(),
            "a refusal must never persist exec_started"
        );

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, Some(0), Some(0)))
                .await
                .expect("start genuine replay stream");
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(0),
                ..
            }))
        ));
        assert!(body.next().await.is_none());
        assert_eq!(exec_completed_statuses(&meta, session_id), vec![Some(0)]);
        // The real run's start WAS recorded — the refusal did not consume
        // the dedup slot.
        assert_eq!(
            exec_started_ids(&meta, session_id),
            vec![exec_id.to_string()],
            "the genuine run must record exactly one exec_started"
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

    /// A silent command (the ADR's non-tty `git clone`) delivers its Exit as
    /// the FIRST coordinator-level frame — agentd's wire `Started` never
    /// reaches this layer. The deferred `exec_started` must therefore be
    /// stamped with the ATTACH time, not the first-frame delivery time, or
    /// `wall_ms` collapses to ~0 for exactly the exec shape ADR 0103 exists
    /// to make durable.
    #[tokio::test]
    async fn silent_exec_wall_spans_from_attach_not_first_frame() {
        let (state, meta, clock) = exec_test_state([vec![ExecEvent::Exit(Some(0))]]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let exec_id = "exec:silent-wall";

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, None, None))
                .await
                .expect("start exec stream");
        // The command runs silently for 45s before its journal Exit is the
        // first (and only) frame this attach delivers.
        clock.advance(Duration::from_secs(45));
        match body.next().await {
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(0),
                rusage,
            })) => assert_eq!(
                rusage.wall_ms, 45_000,
                "wall_ms must span from attach, not from the first delivered frame"
            ),
            other => panic!("expected the exit, got {other:?}"),
        }
        assert!(body.next().await.is_none());
        assert_eq!(
            exec_started_ids(&meta, session_id),
            vec![exec_id.to_string()]
        );
    }

    /// ADR 0103 "known defect" fix: output recording is
    /// observation-independent. A zero-offset re-attach replays bytes the
    /// log already holds; those bytes still STREAM to the caller but must
    /// not be recorded twice — and a chunk straddling the recorded
    /// high-water mark is trimmed at the RAW byte boundary, so the rows
    /// reconstruct the output exactly once with exact range stamps.
    #[tokio::test]
    async fn zero_offset_replay_records_output_rows_exactly_once() {
        let (state, meta, _clock) = exec_test_state([
            // Attach 1: "hello" lands, then severed without Exit.
            vec![ExecEvent::Stdout(Bytes::from_static(b"hello"))],
            // Attach 2 from offset 0: the journal replays everything plus
            // the tail as ONE straddling chunk, then the real Exit.
            vec![
                ExecEvent::Stdout(Bytes::from_static(b"hello world")),
                ExecEvent::Exit(Some(0)),
            ],
        ]);
        let (session_id, _sandbox_id) = stage_exec_session(&meta).await;
        let exec_id = "exec:record-once";

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, None, None))
                .await
                .expect("start exec stream");
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Stdout(bytes))) if bytes == b"hello"
        ));
        assert!(matches!(
            body.next().await,
            Some(Err(ApiError::Unavailable(_)))
        ));
        assert!(body.next().await.is_none());

        let (_id, mut body) =
            exec_stream_core(&state, session_id, durable_req(exec_id, Some(0), Some(0)))
                .await
                .expect("re-attach exec stream");
        // The caller asked for byte 0, so the full replay still streams.
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Stdout(bytes))) if bytes == b"hello world"
        ));
        assert!(matches!(
            body.next().await,
            Some(Ok(ExecStreamEvent::Exit {
                exit_status: Some(0),
                ..
            }))
        ));
        assert!(body.next().await.is_none());

        let rows: Vec<(String, u64, u64)> = meta.with_db(|db| {
            db.session_events
                .get(&session_id)
                .into_iter()
                .flatten()
                .filter(|event| {
                    event.kind == "stdout" && event.payload["exec_id"].as_str() == Some(exec_id)
                })
                .map(|event| {
                    (
                        event.payload["chunk"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        event.payload["bytes_start"].as_u64().unwrap_or(u64::MAX),
                        event.payload["bytes_end"].as_u64().unwrap_or(u64::MAX),
                    )
                })
                .collect()
        });
        let assembled: String = rows.iter().map(|(chunk, _, _)| chunk.as_str()).collect();
        assert_eq!(
            assembled, "hello world",
            "recorded rows must reconstruct the output exactly once, got rows {rows:?}"
        );
        assert_eq!(
            rows,
            vec![("hello".to_string(), 0, 5), (" world".to_string(), 5, 11),],
            "the straddling replay chunk must be trimmed at the recorded high-water mark"
        );
    }
}
