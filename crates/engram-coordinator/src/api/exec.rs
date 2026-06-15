//! `POST /sessions/:id/exec` (sync) and `POST /sessions/:id/exec/stream`
//! (SSE).
//!
//! Both go through the same SandboxBackend `exec_stream`. The sync
//! endpoint drains the stream into buffered stdout/stderr; the SSE
//! endpoint forwards each event to the client *and* publishes it to
//! the session-wide bus so other observers (browser tabs, Slack bots)
//! see it too.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
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
    if let Some(b) = bundle.as_ref() {
        crate::api::sessions::inject_forge_env(state, id, b.manifest.git.as_ref(), &mut env).await;
    }
    // ADR 0026: upload token is not git-gated and doesn't need the
    // manifest bundle — inject it unconditionally so `engram-share`
    // works from `/exec` even when the bundle load above failed.
    crate::api::sessions::inject_upload_env(state, id, &mut env).await;
    (env, bundle.and_then(|b| b.manifest.workdir))
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
/// coordinator-side wall-time rusage the SSE/sync paths compute.
pub(crate) enum ExecStreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit {
        exit_status: Option<i32>,
        rusage: ExecRusage,
    },
}

/// gRPC `Exec` core. Resolves the exec env, auto-resumes the session,
/// kicks off the backend exec stream, and returns `(exec_id, body)` where
/// `body` yields stdout/stderr chunks then a terminal `Exit` — persisting
/// each to the session bus exactly as the SSE handler does.
pub(crate) async fn exec_stream_core(
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
    let (argv, sandbox_req) = build_exec(req, id, base_env, default_workdir)?;

    crate::api::snapshot::ensure_active(state, id).await?;
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
    let body = async_stream::stream! {
        let mut events = backend_stream.events;
        let mut exit_status = None;
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
                    break;
                }
            }
        }
        let rusage = ExecRusage {
            wall_ms: started_at.elapsed().as_millis() as u64,
            ..ExecRusage::default()
        };
        let _ = state_for_stream
            .emit(id, SessionEvent::ExecCompleted {
                exec_id: exec_id_for_stream.clone(),
                exit_status,
                rusage,
                at: Utc::now(),
            })
            .await
            .map_err(|e| tracing::warn!(error = %e, "exec_completed event persistence failed"));
        yield Ok(ExecStreamEvent::Exit { exit_status, rusage });
    };

    Ok((exec_id, Box::pin(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
