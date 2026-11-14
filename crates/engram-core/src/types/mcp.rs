//! Coordinator-owned MCP persistence records. No plaintext credential fields.
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSealedState {
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}

impl std::fmt::Debug for McpSealedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpSealedState([redacted])")
    }
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

#[cfg(test)]
mod tests {
    use super::McpSealedState;

    #[test]
    fn sealed_state_debug_redacts_all_envelope_fields() {
        let state = McpSealedState {
            wrapped_dek: vec![11; 32],
            nonce: vec![22; 12],
            ciphertext: vec![33; 64],
            key_id: "test-key-id".into(),
        };
        assert_eq!(format!("{state:?}"), "McpSealedState([redacted])");
        assert_eq!(format!("{state:#?}"), "McpSealedState([redacted])");
    }
}
