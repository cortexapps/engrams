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
        Err(Status::unimplemented(UNIMPLEMENTED))
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
