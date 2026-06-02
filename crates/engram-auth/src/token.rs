//! Seal/open per-user credentials (the Claude Code OAuth token) under the
//! deployment KEK. Thin wrappers over [`engram_crypto::CredCipher`] that
//! convert between [`SealedCred`](engram_crypto::SealedCred) and the
//! [`UserToken`](engram_core::types::user::UserToken) row shape.

use chrono::Utc;
use engram_core::types::user::UserToken;
use engram_core::UserId;
use engram_crypto::{CredCipher, CryptoError, MasterKeyProvider, SealedCred};

/// Seal a plaintext token into a `UserToken` row (envelope-encrypted).
pub async fn seal_user_token(
    kek: &dyn MasterKeyProvider,
    user_id: UserId,
    kind: &str,
    plaintext: &str,
) -> Result<UserToken, CryptoError> {
    let sealed = CredCipher::new(kek).seal(plaintext.as_bytes()).await?;
    Ok(UserToken {
        user_id,
        kind: kind.to_string(),
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
        created_at: Utc::now(),
        updated_at: None,
    })
}

/// Open a sealed `UserToken` row back into the plaintext token.
pub async fn open_user_token(
    kek: &dyn MasterKeyProvider,
    token: &UserToken,
) -> Result<String, CryptoError> {
    let nonce: [u8; 12] = token
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::DekLength(token.nonce.len()))?;
    let sealed = SealedCred {
        wrapped_dek: token.wrapped_dek.clone(),
        nonce,
        ciphertext: token.ciphertext.clone(),
        key_id: token.key_id.clone(),
    };
    let bytes = CredCipher::new(kek).open(&sealed).await?;
    String::from_utf8(bytes).map_err(|_| CryptoError::Aead("utf8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_crypto::EnvVarKeyProvider;

    #[tokio::test]
    async fn seal_open_round_trip() {
        let kek = EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1");
        let uid = UserId::new();
        let sealed = seal_user_token(&kek, uid, UserToken::KIND_CLAUDE_OAUTH, "sk-oauth-xyz")
            .await
            .unwrap();
        assert_eq!(sealed.user_id, uid);
        assert_eq!(sealed.kind, "claude_oauth");
        assert_ne!(sealed.ciphertext, b"sk-oauth-xyz");
        let opened = open_user_token(&kek, &sealed).await.unwrap();
        assert_eq!(opened, "sk-oauth-xyz");
    }
}
