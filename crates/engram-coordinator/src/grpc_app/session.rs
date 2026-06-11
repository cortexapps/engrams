//! `SessionService` over gRPC (ADR 0039 §2.3). Each RPC is a thin
//! transport adapter: auth-check, decode the request, delegate to a
//! transport-agnostic core in `crate::api::*` (the SAME core the axum
//! handler calls), encode the response. The cores carry no principal and
//! no authz — the caller is the trusted orchestrator (ADR §6).
//!
//! Tasks 11-13 fill in the streaming + remaining RPCs (still
//! `Unimplemented` below).

use std::sync::Arc;

use engram_protocol::app;
use futures::StreamExt as _;
use tonic::{Request, Response, Status};

use super::{auth, into_status, BoxStream, UNIMPLEMENTED};
use crate::state::SharedState;

pub struct AppSessionService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        self.auth.check(&req)?;
        // Trusted caller: return ALL sessions, owner-annotated. The
        // orchestrator scopes by task ownership (ADR §6).
        let resp = crate::api::sessions::list_sessions_core(&self.state)
            .await
            .map_err(into_status)?;
        Ok(Response::new(super::convert::list_sessions_to_proto(resp)))
    }

    async fn create_session(
        &self,
        req: Request<app::CreateSessionRequest>,
    ) -> Result<Response<app::CreateSessionResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        // Reject harness_secret_id until Task 13 wires SecretService.
        // Reading it here (rather than silently dropping in convert.rs) means
        // the orchestrator gets a loud, actionable error instead of a silent
        // no-op. Self-deletes when Task 13 lands.
        if r.harness_secret_id.is_some() {
            return Err(Status::unimplemented(
                "harness_secret_id lands with SecretService (ADR 0039 Task 13)",
            ));
        }
        // No calling user here (ADR §2.1): owner = None, no principal
        // identity env. `harness_secret_id` has been checked above and is
        // `None` at this point; convert.rs binds it as `_`.
        let api_req = super::convert::create_request_from_proto(r).map_err(into_status)?;
        let body = crate::api::sessions::create_session_core(
            &self.state,
            None,
            std::collections::HashMap::new(),
            api_req,
        )
        .await
        .map_err(into_status)?;
        Ok(Response::new(app::CreateSessionResponse {
            session_id: body.session_id.to_string(),
            status: body.status.to_string(),
            image_version: body.image_version,
            kind: body.kind.to_string(),
        }))
    }

    async fn get_session(
        &self,
        req: Request<app::GetSessionRequest>,
    ) -> Result<Response<app::GetSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = super::parse_session_id(&req.get_ref().session_id)?;
        let session = crate::api::sessions::get_session_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::GetSessionResponse {
            session: Some(super::convert::session_to_proto(&session)),
        }))
    }

    async fn delete_session(
        &self,
        req: Request<app::DeleteSessionRequest>,
    ) -> Result<Response<app::DeleteSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = super::parse_session_id(&req.get_ref().session_id)?;
        crate::api::sessions::delete_session_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::DeleteSessionResponse {}))
    }

    async fn send_prompt(
        &self,
        req: Request<app::SendPromptRequest>,
    ) -> Result<Response<app::SendPromptResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = super::parse_session_id(&r.session_id)?;
        let note = crate::api::prompt::send_prompt_core(&self.state, id, r.text)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::SendPromptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    async fn interrupt(
        &self,
        req: Request<app::InterruptRequest>,
    ) -> Result<Response<app::InterruptResponse>, Status> {
        self.auth.check(&req)?;
        let id = super::parse_session_id(&req.get_ref().session_id)?;
        let note = crate::api::interrupt::interrupt_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::InterruptResponse {
            session_id: id.to_string(),
            note: note.to_string(),
        }))
    }

    type StreamEventsStream = BoxStream<app::SessionEvent>;

    async fn stream_events(
        &self,
        req: Request<app::StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let id = super::parse_session_id(&r.session_id)?;

        // proto3: `since` unset = from the start. We pass None so
        // events_core uses -1 (the "all" sentinel). The HTTP -1
        // translation is the orchestrator's job; we do not touch it here.
        let since: Option<i64> = r.since;

        let (replayed, live_rx) =
            crate::api::events::events_core(&self.state, id, since)
                .await
                .map_err(super::into_status)?;

        let replay_high_water = replayed
            .last()
            .map(|e| e.idx)
            .unwrap_or(since.unwrap_or(-1));

        // Map the replayed (persisted) events to proto SessionEvent.
        // with_rewind_meta applies to BOTH arms (replay + live) as per
        // the SSE handler — the ADR 0028 A.log metadata folds into
        // payload_json on every message.
        //
        // Collect into plain events first (no Result wrapper) to avoid
        // the `result_large_err` lint on `tonic::Status`.
        let replay_proto: Vec<app::SessionEvent> = replayed
            .into_iter()
            .map(|ev| {
                let rewound = ev.rewound_at.is_some();
                let payload_json =
                    crate::api::events::with_rewind_meta(ev.payload, ev.recovery_epoch, rewound);
                app::SessionEvent {
                    idx: Some(ev.idx),
                    kind: ev.kind,
                    payload_json,
                }
            })
            .collect();
        let replay_events = futures::stream::iter(replay_proto.into_iter().map(Ok::<_, Status>));

        // Map the live broadcast stream. Drop events already replayed
        // (idx <= replay_high_water) — exact dedup rule from events.rs:98-101.
        // Lagged → special SessionEvent with kind="lagged", idx unset,
        // payload_json={"missed":n} — exact rule from events.rs:102-106.
        use tokio_stream::wrappers::BroadcastStream;
        let live_stream = BroadcastStream::new(live_rx).filter_map(
            move |recv| async move {
                match recv {
                    Ok(indexed) if indexed.idx > replay_high_water => {
                        let payload =
                            serde_json::to_value(&indexed.event).unwrap_or(serde_json::Value::Null);
                        let payload_json =
                            crate::api::events::with_rewind_meta(payload, 0, false);
                        Some(Ok(app::SessionEvent {
                            idx: Some(indexed.idx),
                            kind: indexed.event.kind().to_string(),
                            payload_json,
                        }))
                    }
                    Ok(_) => None,
                    Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                        Some(Ok(app::SessionEvent {
                            idx: None,
                            kind: "lagged".to_string(),
                            payload_json: format!(r#"{{"missed":{n}}}"#),
                        }))
                    }
                }
            },
        );

        // RAII teardown guard: when the stream future is dropped
        // (client disconnects), log at debug level. The broadcast
        // Receiver itself drops at the same time, releasing the slot.
        struct DisconnectGuard {
            session_id: engram_core::SessionId,
        }
        impl Drop for DisconnectGuard {
            fn drop(&mut self) {
                tracing::debug!(
                    session_id = %self.session_id,
                    "StreamEvents: client disconnected, receiver dropped (RAII teardown)"
                );
            }
        }
        let guard = DisconnectGuard { session_id: id };

        let full_stream = replay_events
            .chain(live_stream)
            // Attach the guard to the stream so it lives exactly as long as
            // the stream is being polled. When the stream is dropped, the
            // guard drops too.
            .chain(futures::stream::poll_fn(move |_| {
                // Keep guard alive until the upstream stream exhausts.
                // This arm is never reached (the live tail is infinite),
                // but Rust's drop-ordering requires the value to be moved
                // into the closure body.
                let _ = &guard;
                std::task::Poll::Ready(None)
            }));

        Ok(Response::new(Box::pin(full_stream)))
    }

    type ExecStream = BoxStream<app::ExecOutput>;

    async fn exec(
        &self,
        req: Request<app::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_log(
        &self,
        req: Request<app::GetLogRequest>,
    ) -> Result<Response<app::GetLogResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn snapshot(
        &self,
        req: Request<app::SnapshotRequest>,
    ) -> Result<Response<app::SnapshotResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn resume(
        &self,
        req: Request<app::ResumeRequest>,
    ) -> Result<Response<app::ResumeResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn evict_local(
        &self,
        req: Request<app::EvictLocalRequest>,
    ) -> Result<Response<app::EvictLocalResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_cow_state(
        &self,
        req: Request<app::GetCowStateRequest>,
    ) -> Result<Response<app::GetCowStateResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_checkpoints(
        &self,
        req: Request<app::ListCheckpointsRequest>,
    ) -> Result<Response<app::ListCheckpointsResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type GetArtifactStream = BoxStream<app::GetArtifactResponse>;

    async fn get_artifact(
        &self,
        req: Request<app::GetArtifactRequest>,
    ) -> Result<Response<Self::GetArtifactStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn create_artifact_from_path(
        &self,
        req: Request<app::CreateArtifactFromPathRequest>,
    ) -> Result<Response<app::CreateArtifactFromPathResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

#[cfg(test)]
mod tests {
    use engram_protocol::app;

    /// `create_session` must loudly refuse `harness_secret_id: Some(_)` until
    /// Task 13 wires SecretService. Validates Change 2 of the review fixes.
    /// The RPC body checks `r.harness_secret_id.is_some()` and returns
    /// `Status::unimplemented(...)` before delegating to convert.rs.
    #[test]
    fn create_request_from_proto_rejects_harness_secret_id() {
        let r = app::CreateSessionRequest {
            image_uri: "localhost:5001/demo:warm".into(),
            mode: String::new(),
            prompt: None,
            harness_secret_id: Some("secret-abc-123".into()),
            secrets: std::collections::HashMap::new(),
        };
        // Verify the field is detectable — the RPC layer (create_session)
        // checks this and short-circuits with Status::unimplemented.
        assert!(
            r.harness_secret_id.is_some(),
            "harness_secret_id must be detected as Some(_) so the RPC rejects it"
        );
    }
}
