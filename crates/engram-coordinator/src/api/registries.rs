//! `POST/GET/DELETE /api/registries` — manage Docker registry
//! credentials. The password is sealed via `engram-crypto::CredCipher`
//! before it touches Postgres; reads (e.g. for host-agent pulls) go
//! through the same path in reverse.
//!
//! `GET /api/registries` never returns the password (or its
//! ciphertext) — only the host, username, key_id, and timestamps.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::registry::{RegistryCredential, RegistryCredentialSummary};
use engram_core::MetaError;
use engram_crypto::CredCipher;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct AddRegistryRequest {
    pub host: String,
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct AddRegistryResponse {
    pub id: Uuid,
    pub host: String,
    pub username: String,
    pub key_id: String,
}

#[derive(Serialize)]
pub struct ListRegistriesResponse {
    pub registries: Vec<RegistryCredentialSummary>,
}

pub async fn add_registry(
    State(state): State<SharedState>,
    Json(req): Json<AddRegistryRequest>,
) -> Result<(StatusCode, Json<AddRegistryResponse>), ApiError> {
    if req.host.trim().is_empty() {
        return Err(ApiError::BadRequest("host must not be empty".into()));
    }
    if req.username.trim().is_empty() {
        return Err(ApiError::BadRequest("username must not be empty".into()));
    }
    if req.password.is_empty() {
        return Err(ApiError::BadRequest("password must not be empty".into()));
    }

    let cipher = CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher
        .seal(req.password.as_bytes())
        .await
        .map_err(|e| ApiError::Internal(format!("seal credential: {e}")))?;

    let cred = RegistryCredential {
        id: Uuid::new_v4(),
        registry_host: req.host.clone(),
        username: req.username.clone(),
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id.clone(),
        created_at: Utc::now(),
        updated_at: None,
    };

    state
        .services
        .meta
        .upsert_registry_credential(cred.clone())
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(AddRegistryResponse {
            id: cred.id,
            host: cred.registry_host,
            username: cred.username,
            key_id: cred.key_id,
        }),
    ))
}

pub async fn list_registries(
    State(state): State<SharedState>,
) -> Result<Json<ListRegistriesResponse>, ApiError> {
    let creds = state.services.meta.list_registry_credentials().await?;
    let registries = creds
        .into_iter()
        .map(RegistryCredentialSummary::from)
        .collect();
    Ok(Json(ListRegistriesResponse { registries }))
}

pub async fn delete_registry(
    State(state): State<SharedState>,
    Path(host): Path<String>,
) -> Result<StatusCode, ApiError> {
    match state.services.meta.delete_registry_credential(&host).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(MetaError::NotFound) => Err(ApiError::NotFound(format!(
            "no registry credential for host {host:?}"
        ))),
        Err(e) => Err(e.into()),
    }
}
