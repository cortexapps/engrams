//! ADR 0057: `OrgSecretService` over app-gRPC — the admin-managed, KEK-sealed
//! org secret store. The caller is the trusted orchestrator (bearer-authed);
//! per-user authz (admin-only writes) lives there. Values are sealed at the
//! coordinator on write ([`crate::org_secrets::seal_org_secret`]) and are NEVER
//! returned — `ListSecrets` surfaces metadata only ("set / not set").

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, into_status};
use crate::error::ApiError;
use crate::state::SharedState;

pub struct AppOrgSecretService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

fn meta_to_proto(s: engram_core::types::org_secret::OrgSecret) -> app::OrgSecretMeta {
    app::OrgSecretMeta {
        name: s.name,
        key_id: s.key_id,
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::org_secret_service_server::OrgSecretService for AppOrgSecretService {
    async fn put_secret(
        &self,
        req: Request<app::PutSecretRequest>,
    ) -> Result<Response<app::PutSecretResponse>, Status> {
        self.auth.check(&req)?;
        let app::PutSecretRequest { name, value } = req.into_inner();
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(into_status(ApiError::BadRequest(
                "org secret name must not be empty".into(),
            )));
        }
        // Seal at the coordinator — the plaintext value never persists outside
        // the KEK envelope, and is never echoed back.
        let sealed =
            crate::org_secrets::seal_org_secret(&*self.state.services.kek, &name, value.as_bytes())
                .await
                .map_err(|e| into_status(ApiError::Internal(format!("seal org secret: {e}"))))?;
        let row = self
            .state
            .services
            .meta
            .upsert_org_secret(sealed)
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        tracing::info!(name = %row.name, key_id = %row.key_id, "upserted org secret");
        Ok(Response::new(app::PutSecretResponse {
            secret: Some(meta_to_proto(row)),
        }))
    }

    async fn list_secrets(
        &self,
        req: Request<app::ListSecretsRequest>,
    ) -> Result<Response<app::ListSecretsResponse>, Status> {
        self.auth.check(&req)?;
        let rows = self
            .state
            .services
            .meta
            .list_org_secrets()
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        Ok(Response::new(app::ListSecretsResponse {
            secrets: rows.into_iter().map(meta_to_proto).collect(),
        }))
    }

    async fn delete_secret(
        &self,
        req: Request<app::DeleteSecretRequest>,
    ) -> Result<Response<app::DeleteSecretResponse>, Status> {
        self.auth.check(&req)?;
        let name = req.into_inner().name;
        let deleted = self
            .state
            .services
            .meta
            .delete_org_secret(&name)
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        Ok(Response::new(app::DeleteSecretResponse { deleted }))
    }
}
