//! `IntegrationOpService` over app-gRPC — server-side, sessionless integration
//! invocation (the "IntegrationOp" seam). The caller is the trusted orchestrator
//! (bearer-authed); per-user authz (admin-only) lives there. The coordinator is
//! the only tier that can unseal org secrets + run the mint engine, so it executes
//! from a resolved spec the orchestrator builds — exactly like `MintService`.
//!
//! The real logic lives in `crate::integration_ops` (the auth-convention test in
//! `grpc_app/mod.rs` counts one `self.auth.check` per `async fn` in this file, so
//! these methods stay thin: check + delegate).

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::auth;
use crate::state::SharedState;

pub struct AppIntegrationOpService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::integration_op_service_server::IntegrationOpService for AppIntegrationOpService {
    async fn run_integration_op(
        &self,
        req: Request<app::RunIntegrationOpRequest>,
    ) -> Result<Response<app::RunIntegrationOpResponse>, Status> {
        self.auth.check(&req)?;
        let resp = crate::integration_ops::run_integration_op(&self.state, req.into_inner())
            .await
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(resp))
    }

    async fn resolve_integration_credential(
        &self,
        req: Request<app::ResolveIntegrationCredentialRequest>,
    ) -> Result<Response<app::ResolveIntegrationCredentialResponse>, Status> {
        self.auth.check(&req)?;
        let credential =
            crate::integration_ops::resolve_integration_credential(&self.state, req.into_inner())
                .await
                .map_err(Status::failed_precondition)?;
        Ok(Response::new(app::ResolveIntegrationCredentialResponse {
            credential: Some(credential),
        }))
    }

    async fn begin_integration_oauth(
        &self,
        req: Request<app::BeginIntegrationOauthRequest>,
    ) -> Result<Response<app::BeginIntegrationOauthResponse>, Status> {
        self.auth.check(&req)?;
        let authorize_url =
            crate::integration_ops::begin_integration_oauth(&self.state, req.into_inner())
                .await
                .map_err(Status::failed_precondition)?;
        Ok(Response::new(app::BeginIntegrationOauthResponse {
            authorize_url,
        }))
    }

    async fn complete_integration_oauth(
        &self,
        req: Request<app::CompleteIntegrationOauthRequest>,
    ) -> Result<Response<app::CompleteIntegrationOauthResponse>, Status> {
        self.auth.check(&req)?;
        let (ok, message) =
            crate::integration_ops::complete_integration_oauth(&self.state, req.into_inner())
                .await
                .map_err(Status::failed_precondition)?;
        Ok(Response::new(app::CompleteIntegrationOauthResponse {
            ok,
            message,
        }))
    }
}
