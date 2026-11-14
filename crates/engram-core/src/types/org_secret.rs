//! ADR 0057: org-managed secrets — admin-entered, KEK-sealed credential
//! values stored in the coordinator's Postgres and resolved through the
//! composed [`SecretStore`](crate::traits::SecretStore) (the org backend
//! layered in front of the deployment backend).
//!
//! Two shapes, mirroring the [`SessionBrokerToken`](crate::types::registry::SessionBrokerToken)
//! envelope: [`SealedOrgSecret`] is what crosses to/from Postgres (ciphertext
//! only — the value never appears); [`OrgSecret`] is the *listable* metadata
//! (name + key id + timestamps, never a value) the admin UI renders as
//! "set / not set".

use chrono::{DateTime, Utc};

/// A KEK-sealed org secret row. The plaintext value is never carried — only
/// the AES-GCM envelope (`wrapped_dek` + `nonce` + `ciphertext`) and the KEK
/// `key_id` in effect at seal time, so a rotation can be detected. Mirrors
/// [`SessionBrokerToken`](crate::types::registry::SessionBrokerToken), keyed
/// by `name` instead of a session id.
#[derive(Clone, Debug)]
pub struct SealedOrgSecret {
    pub name: String,
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}

/// Listable metadata for an org secret — everything *except* the value.
/// `OrgSecretService.ListSecrets` returns these so the UI can show
/// "set / not set" without ever echoing a credential back.
#[derive(Clone, Debug)]
pub struct OrgSecret {
    pub name: String,
    pub key_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The only two uses for reserved organization credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSecretRole {
    ApiKey,
    ClientSecret,
}
impl McpSecretRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::ClientSecret => "client_secret",
        }
    }
}

/// Permanent ownership, with a fence that changes on every value mutation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpSecretReservation {
    pub name: String,
    pub connection_id: String,
    pub role: McpSecretRole,
    pub generation: i64,
}
