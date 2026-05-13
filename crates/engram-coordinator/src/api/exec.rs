//! `POST /sessions/:id/exec` (sync) and `POST /sessions/:id/exec/stream`
//! (SSE).
//!
//! Both go through the same SandboxBackend `exec_stream`. The sync
//! endpoint drains the stream into buffered stdout/stderr; the SSE
//! endpoint forwards each event to the client *and* publishes it to
//! the session-wide bus so other observers (browser tabs, Slack bots)
//! see it too.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use chrono::Utc;
use engram_core::types::{ExecEvent, ExecRequest as SandboxExecRequest, ExecRusage};
use engram_core::SessionId;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

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
}

#[derive(Serialize)]
pub struct ExecResponse {
    pub session_id: SessionId,
    pub exec_id: String,
    pub exit_status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub rusage: ExecRusage,
}

/// Helper: validate the request shape and produce an argv + sandbox
/// ExecRequest. Centralised so the sync and streaming endpoints stay
/// consistent.
///
/// `ENGRAM_SESSION_ID` is always injected last so user-supplied env
/// can't accidentally clobber it. Pool-served sandboxes don't have
/// a session id baked into their spec — this is where it lands.
fn build_exec(
    req: ExecRequest,
    session: SessionId,
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
    let mut env = req.env;
    env.insert("ENGRAM_SESSION_ID".into(), session.to_string());
    let sandbox_req = SandboxExecRequest {
        command: argv.clone(),
        stdin: None,
        env,
        workdir: req.workdir,
        timeout: req.timeout_secs.map(Duration::from_secs),
    };
    Ok((argv, sandbox_req))
}

pub async fn exec(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, ApiError> {
    let (argv, sandbox_req) = build_exec(req, id)?;

    // Track B: idle sessions transparently auto-resume on the next
    // request. The exec handler doesn't need to know whether the
    // session was hot-suspended a few seconds ago — `ensure_active`
    // handles the FC restore (or git-checkpoint cold resume) and
    // returns once the session is Active again.
    crate::api::snapshot::ensure_active(&state, id).await?;

    let sandbox_id = state.registry.get(id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox — create a new session or resume from snapshot".into(),
        )
    })?;

    let stream = state
        .services
        .host
        .exec_stream(sandbox_id, sandbox_req)
        .await?;
    let exec_id = stream.exec_id.clone();

    // Coordinator-side wall-time measurement. Captured around the
    // backend.exec_stream → final Exit event window. peak_rss_kb /
    // user_cpu_ms / sys_cpu_ms stay None for ProcessBackend; future
    // backends (Firecracker, cgroup-aware) populate them via the
    // ExecStream surface.
    let started_at = Instant::now();
    state
        .emit(
            id,
            SessionEvent::ExecStarted {
                exec_id: exec_id.clone(),
                command: argv.clone(),
                at: Utc::now(),
            },
        )
        .await?;

    let mut events = stream.events;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_status = None;
    while let Some(ev) = events.next().await {
        match ev {
            ExecEvent::Stdout(b) => {
                let chunk_str = String::from_utf8_lossy(&b).into_owned();
                stdout.extend_from_slice(&b);
                state
                    .emit(
                        id,
                        SessionEvent::Stdout {
                            exec_id: exec_id.clone(),
                            chunk: chunk_str,
                        },
                    )
                    .await?;
            }
            ExecEvent::Stderr(b) => {
                let chunk_str = String::from_utf8_lossy(&b).into_owned();
                stderr.extend_from_slice(&b);
                state
                    .emit(
                        id,
                        SessionEvent::Stderr {
                            exec_id: exec_id.clone(),
                            chunk: chunk_str,
                        },
                    )
                    .await?;
            }
            ExecEvent::Exit(code) => {
                exit_status = code;
                break;
            }
        }
    }

    let rusage = ExecRusage {
        wall_ms: started_at.elapsed().as_millis() as u64,
        ..ExecRusage::default()
    };
    state
        .emit(
            id,
            SessionEvent::ExecCompleted {
                exec_id: exec_id.clone(),
                exit_status,
                rusage,
                at: Utc::now(),
            },
        )
        .await?;

    Ok(Json(ExecResponse {
        session_id: id,
        exec_id,
        exit_status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        rusage,
    }))
}

/// Streaming exec: returns SSE so each stdout/stderr chunk arrives at
/// the client as the underlying process produces it. Also publishes
/// every event into the session-wide bus so other subscribers see it.
pub async fn exec_stream(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<ExecRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let (argv, sandbox_req) = build_exec(req, id)?;

    crate::api::snapshot::ensure_active(&state, id).await?;
    let sandbox_id = state.registry.get(id).ok_or_else(|| {
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

    state
        .emit(
            id,
            SessionEvent::ExecStarted {
                exec_id: exec_id.clone(),
                command: argv.clone(),
                at: Utc::now(),
            },
        )
        .await?;

    let state_for_stream = state.clone();
    let exec_id_for_stream = exec_id.clone();
    let started_at = Instant::now();
    let sse_stream = async_stream::stream! {
        let mut events = backend_stream.events;
        let mut exit_status = None;
        while let Some(ev) = events.next().await {
            match ev {
                ExecEvent::Stdout(bytes) => {
                    let chunk = String::from_utf8_lossy(&bytes).into_owned();
                    // Persist + broadcast to other subscribers; if the
                    // DB write fails we log and keep streaming so the
                    // active client doesn't lose the live tail. The
                    // returned idx labels the SSE message so reconnects
                    // can resume after this point.
                    let idx = state_for_stream
                        .emit(
                            id,
                            SessionEvent::Stdout {
                                exec_id: exec_id_for_stream.clone(),
                                chunk: chunk.clone(),
                            },
                        )
                        .await
                        .map_err(|e| {
                            tracing::warn!(error = %e, "stdout event persistence failed; live tail continues");
                            e
                        })
                        .ok();
                    let payload = serde_json::json!({
                        "exec_id": exec_id_for_stream,
                        "chunk": chunk,
                    })
                    .to_string();
                    let mut sse = Event::default().event("stdout").data(payload);
                    if let Some(idx) = idx { sse = sse.id(idx.to_string()); }
                    yield Ok::<Event, Infallible>(sse);
                }
                ExecEvent::Stderr(bytes) => {
                    let chunk = String::from_utf8_lossy(&bytes).into_owned();
                    let idx = state_for_stream
                        .emit(
                            id,
                            SessionEvent::Stderr {
                                exec_id: exec_id_for_stream.clone(),
                                chunk: chunk.clone(),
                            },
                        )
                        .await
                        .map_err(|e| {
                            tracing::warn!(error = %e, "stderr event persistence failed; live tail continues");
                            e
                        })
                        .ok();
                    let payload = serde_json::json!({
                        "exec_id": exec_id_for_stream,
                        "chunk": chunk,
                    })
                    .to_string();
                    let mut sse = Event::default().event("stderr").data(payload);
                    if let Some(idx) = idx { sse = sse.id(idx.to_string()); }
                    yield Ok(sse);
                }
                ExecEvent::Exit(code) => {
                    exit_status = code;
                    break;
                }
            }
        }
        let rusage = ExecRusage {
            wall_ms: started_at.elapsed().as_millis() as u64,
            ..ExecRusage::default()
        };
        let exit_idx = state_for_stream
            .emit(
                id,
                SessionEvent::ExecCompleted {
                    exec_id: exec_id_for_stream.clone(),
                    exit_status,
                    rusage,
                    at: Utc::now(),
                },
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "exec_completed event persistence failed");
                e
            })
            .ok();
        let payload = serde_json::json!({
            "exec_id": exec_id_for_stream,
            "exit_status": exit_status,
            "rusage": rusage,
        })
        .to_string();
        let mut sse = Event::default().event("exit").data(payload);
        if let Some(idx) = exit_idx { sse = sse.id(idx.to_string()); }
        yield Ok(sse);
    };

    Ok(Sse::new(sse_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}
