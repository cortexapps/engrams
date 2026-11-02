//! Coordinator-owned MCP persistence records. No plaintext credential fields.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSealedState {
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpRefreshClaim {
    pub id: uuid::Uuid,
    pub key: super::oauth::OAuthCredentialKey,
    pub version: i64,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthAdmission {
    pub id: uuid::Uuid,
    pub deployment_id: String,
    pub connection_id: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}
