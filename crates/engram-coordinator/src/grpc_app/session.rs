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

use super::{auth, BoxStream, UNIMPLEMENTED};
use crate::error::ApiError;
use crate::state::SharedState;

/// Map an [`ApiError`] to a gRPC [`Status`], exhaustively over every
/// variant (so a new `ApiError` arm is a compile error here, not a
/// silent `internal`). The HTTP→gRPC code mapping follows the standard
/// google.rpc.Code correspondence; the [`ApiError::slug`] rides along as
/// `engram-error-slug` Status metadata so the web can distinguish slugs
/// that share an HTTP code (`snapshot_invalidated` vs `host_lost`, both
/// 410) and the orchestrator can relay it.
pub(super) fn into_status(err: ApiError) -> Status {
    use tonic::Code;
    let code = match err {
        ApiError::NotFound(_) => Code::NotFound,
        ApiError::Forbidden(_) => Code::PermissionDenied,
        ApiError::Unauthorized(_) => Code::Unauthenticated,
        ApiError::BadRequest(_) => Code::InvalidArgument,
        ApiError::Conflict(_) => Code::FailedPrecondition,
        ApiError::Gone(_) | ApiError::HostLost(_) => Code::FailedPrecondition,
        ApiError::Unavailable(_) => Code::Unavailable,
        ApiError::Unsupported(_) => Code::Unimplemented,
        ApiError::PayloadTooLarge(_) | ApiError::TooManyRequests(_) => Code::ResourceExhausted,
        ApiError::Internal(_) => Code::Internal,
    };
    let slug = err.slug();
    let mut status = Status::new(code, err.message().to_string());
    // Best-effort: the slug is a static ASCII identifier, so the parse
    // never fails; guard anyway so a future non-ASCII slug can't panic
    // the RPC.
    if let Ok(val) = slug.parse() {
        status.metadata_mut().insert("engram-error-slug", val);
    }
    status
}

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
        // No calling user here (ADR §2.1): owner = None, no principal
        // identity env. `harness_secret_id` unsealing lands in Task 13
        // with SecretService; until then gRPC create works for
        // no-harness / pre-authed images.
        let req = super::convert::create_request_from_proto(r).map_err(into_status)?;
        let body = crate::api::sessions::create_session_core(
            &self.state,
            None,
            std::collections::HashMap::new(),
            req,
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
    use super::*;

    #[test]
    fn into_status_maps_codes_and_attaches_slug() {
        use tonic::Code;
        let cases = [
            (ApiError::NotFound("x".into()), Code::NotFound, "not_found"),
            (
                ApiError::Forbidden("x".into()),
                Code::PermissionDenied,
                "forbidden",
            ),
            (
                ApiError::Unauthorized("x".into()),
                Code::Unauthenticated,
                "unauthorized",
            ),
            (
                ApiError::BadRequest("x".into()),
                Code::InvalidArgument,
                "bad_request",
            ),
            (
                ApiError::Conflict("x".into()),
                Code::FailedPrecondition,
                "conflict",
            ),
            (
                ApiError::Gone("x".into()),
                Code::FailedPrecondition,
                "snapshot_invalidated",
            ),
            (
                ApiError::HostLost("x".into()),
                Code::FailedPrecondition,
                "host_lost",
            ),
            (
                ApiError::Unavailable("x".into()),
                Code::Unavailable,
                "unavailable",
            ),
            (
                ApiError::Unsupported("x".into()),
                Code::Unimplemented,
                "unsupported",
            ),
            (
                ApiError::PayloadTooLarge("x".into()),
                Code::ResourceExhausted,
                "payload_too_large",
            ),
            (
                ApiError::TooManyRequests("x".into()),
                Code::ResourceExhausted,
                "too_many_requests",
            ),
            (ApiError::Internal("x".into()), Code::Internal, "internal"),
        ];
        for (err, code, slug) in cases {
            let st = into_status(err);
            assert_eq!(st.code(), code, "code for slug {slug}");
            assert_eq!(
                st.metadata().get("engram-error-slug").map(|v| v.as_bytes()),
                Some(slug.as_bytes()),
                "slug metadata for {slug}",
            );
            assert_eq!(st.message(), "x");
        }
    }

    // `Gone` and `HostLost` share a gRPC code but carry distinct slugs —
    // the whole reason the slug rides in metadata.
    #[test]
    fn gone_and_host_lost_share_code_but_differ_by_slug() {
        let gone = into_status(ApiError::Gone("g".into()));
        let lost = into_status(ApiError::HostLost("l".into()));
        assert_eq!(gone.code(), lost.code());
        assert_ne!(
            gone.metadata().get("engram-error-slug").unwrap().as_bytes(),
            lost.metadata().get("engram-error-slug").unwrap().as_bytes(),
        );
    }
}
