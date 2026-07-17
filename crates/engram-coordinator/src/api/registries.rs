//! `POST/GET/DELETE /api/registries` — manage Docker registry
//! credentials.
//!
//! The wire shape mirrors `RegistryAuthSpec`'s `serde(tag = "kind")`
//! discriminator: each request supplies an `auth: { kind, ... }`
//! object whose other fields depend on the kind. For `static`, the
//! caller passes the plaintext password and we seal it via
//! `engram-crypto::CredCipher` before it touches Postgres. For
//! cloud-IAM kinds (today: `gcp_workload_identity`) there's no
//! secret material — the runtime's ambient identity is the
//! credential.
//!
//! `GET /api/registries` returns [`RegistryCredentialSummary`] only
//! — never plaintext passwords, never ciphertext bytes.

use engram_core::types::registry::{
    RegistryAuthSpec, RegistryCredential, RegistryCredentialSummary,
};
use engram_crypto::CredCipher;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

/// Wire shape: discriminated on `kind`. Static carries plaintext
/// `password`; cloud-IAM kinds skip it. Validation lives entirely in
/// [`AddRegistryRequest::validate`] so the handler doesn't have to
/// re-pattern-match.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AddRegistryAuth {
    Static {
        username: String,
        /// Plaintext on the wire — sealed under the deployment KEK
        /// before it lands in Postgres. The serialized response
        /// never echoes this field back.
        password: String,
    },
    GcpWorkloadIdentity {
        /// Optional service account to impersonate via IAM
        /// Credentials API. `None` = use ambient identity directly.
        #[serde(default)]
        impersonate_sa: Option<String>,
    },
    /// Public registry — no auth material to store. Stored as a row
    /// so the dashboard's catalog browser can list the host and so
    /// `enabled_images` POST can validate against a known set.
    Anonymous,
}

#[derive(Deserialize)]
pub struct AddRegistryRequest {
    pub host: String,
    pub auth: AddRegistryAuth,
}

#[derive(Serialize)]
pub struct AddRegistryResponse {
    pub id: Uuid,
    pub host: String,
    pub auth_kind: String,
    pub auth_principal: Option<String>,
}

/// ADR 0051: transport-agnostic add-registry core (gRPC `AddRegistry`).
/// Same per-variant validation + sealing as the axum `add_registry`
/// handler — only the return shape changed (`(StatusCode, Json<...>)` →
/// the plain body).
pub(crate) async fn add_registry_core(
    state: &SharedState,
    req: AddRegistryRequest,
) -> Result<AddRegistryResponse, ApiError> {
    if req.host.trim().is_empty() {
        return Err(ApiError::BadRequest("host must not be empty".into()));
    }

    // Per-variant validation + variant->RegistryAuthSpec lift. The
    // static branch seals the password; the cloud-IAM branches store
    // no secret material so they're trivial pass-through.
    let auth = match req.auth {
        AddRegistryAuth::Static { username, password } => {
            if username.trim().is_empty() {
                return Err(ApiError::BadRequest("username must not be empty".into()));
            }
            if password.is_empty() {
                return Err(ApiError::BadRequest("password must not be empty".into()));
            }
            let cipher = CredCipher::new(state.services.kek.as_ref());
            let sealed = cipher
                .seal(password.as_bytes())
                .await
                .map_err(|e| ApiError::Internal(format!("seal credential: {e}")))?;
            RegistryAuthSpec::Static {
                username,
                wrapped_dek: sealed.wrapped_dek,
                nonce: sealed.nonce.to_vec(),
                ciphertext: sealed.ciphertext,
                key_id: sealed.key_id,
            }
        }
        AddRegistryAuth::GcpWorkloadIdentity { impersonate_sa } => {
            // No secret material; the host-agent's ambient GCP
            // identity is the credential. We accept the row even
            // though the host-agent may not actually be running on
            // GCP — that's a pull-time error, not a configuration
            // error, and surfacing it here would block a perfectly
            // valid "configure now, deploy host-agent later" flow.
            RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa }
        }
        AddRegistryAuth::Anonymous => RegistryAuthSpec::Anonymous,
    };

    let cred = RegistryCredential {
        id: state.services.entropy.uuid(),
        registry_host: req.host.clone(),
        auth,
        created_at: state.services.clock.now_utc(),
        updated_at: None,
    };
    state
        .services
        .meta
        .upsert_registry_credential(cred.clone())
        .await?;

    let summary: RegistryCredentialSummary = cred.into();
    Ok(AddRegistryResponse {
        id: summary.id,
        host: summary.registry_host,
        auth_kind: summary.auth_kind,
        auth_principal: summary.auth_principal,
    })
}
