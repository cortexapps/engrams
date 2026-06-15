//! `SecretService` over gRPC (ADR 0039 §2.3). KEK-sealed opaque secret
//! store. Keys are caller-supplied (never interpreted by the coordinator).
//! No GetSecret RPC — plaintext never leaves the coordinator.

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, into_status};
use crate::state::SharedState;

pub struct AppSecretService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::secret_service_server::SecretService for AppSecretService {
    async fn put_secret(
        &self,
        req: Request<app::PutSecretRequest>,
    ) -> Result<Response<app::PutSecretResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        if r.key.is_empty() {
            return Err(Status::invalid_argument("key must not be empty"));
        }
        if r.value.is_empty() {
            return Err(Status::invalid_argument("value must not be empty"));
        }
        // NEVER log r.value — keep the handler's redaction discipline.
        let cipher = engram_crypto::CredCipher::new(self.state.services.kek.as_ref());
        let sealed = cipher.seal(r.value.as_bytes()).await.map_err(|e| {
            into_status(crate::error::ApiError::Internal(format!(
                "seal secret: {e}"
            )))
        })?;
        self.state
            .services
            .meta
            .put_sealed_secret(
                &r.key,
                sealed.wrapped_dek,
                sealed.nonce.to_vec(),
                sealed.ciphertext,
                sealed.key_id,
            )
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        tracing::debug!(key = %r.key, "put_secret: sealed and stored");
        Ok(Response::new(app::PutSecretResponse {}))
    }

    async fn has_secret(
        &self,
        req: Request<app::HasSecretRequest>,
    ) -> Result<Response<app::HasSecretResponse>, Status> {
        self.auth.check(&req)?;
        let key = &req.get_ref().key;
        let exists = self
            .state
            .services
            .meta
            .has_sealed_secret(key)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        Ok(Response::new(app::HasSecretResponse { exists }))
    }

    async fn delete_secret(
        &self,
        req: Request<app::DeleteSecretRequest>,
    ) -> Result<Response<app::DeleteSecretResponse>, Status> {
        self.auth.check(&req)?;
        let key = &req.get_ref().key;
        // Idempotent: deleting a missing key is fine.
        self.state
            .services
            .meta
            .delete_sealed_secret(key)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        Ok(Response::new(app::DeleteSecretResponse {}))
    }
}
