//! Static-credential strategy: decrypt a Postgres-sealed password
//! once, return the resulting basic creds on every `fetch_creds`.
//!
//! The decryption happens in `seal_open` (the constructor) — by the
//! time the strategy is cached in `PgAuthResolver::strategies`, the
//! plaintext is already in memory. Subsequent `fetch_creds` calls
//! return clones of the cached creds without further KMS / KEK
//! interaction.
//!
//! That trade-off is deliberate: the host-agent's process memory is
//! already as trusted as the deployment KEK (it has to be, to
//! exchange wrapped DEKs for plaintext at all). Keeping the decrypted
//! creds in-memory once we've earned the right to see them avoids a
//! per-pull KMS round trip without weakening the threat model.

use async_trait::async_trait;
use engram_crypto::{CredCipher, MasterKeyProvider, SealedCred};
use engram_oci::{BasicCreds, OciError};

use crate::AuthStrategy;

pub struct StaticStrategy {
    creds: BasicCreds,
}

impl StaticStrategy {
    /// Construct by unsealing a `RegistryAuthSpec::Static` row's
    /// cipher fields under the deployment KEK. Errors propagate up
    /// so the resolver surfaces "wrong KEK / corrupted ciphertext"
    /// at strategy-build time rather than per-pull.
    pub async fn seal_open(
        kek: &dyn MasterKeyProvider,
        username: String,
        wrapped_dek: &[u8],
        nonce_bytes: &[u8],
        ciphertext: &[u8],
        key_id: &str,
    ) -> Result<Self, OciError> {
        if nonce_bytes.len() != 12 {
            return Err(OciError::Distribution(format!(
                "static credential nonce must be 12 bytes, got {}",
                nonce_bytes.len()
            )));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(nonce_bytes);
        let sealed = SealedCred {
            wrapped_dek: wrapped_dek.to_vec(),
            nonce,
            ciphertext: ciphertext.to_vec(),
            key_id: key_id.to_string(),
        };
        let plaintext = CredCipher::new(kek)
            .open(&sealed)
            .await
            .map_err(|e| OciError::Distribution(format!("decrypt static cred: {e}")))?;
        let password = String::from_utf8(plaintext)
            .map_err(|e| OciError::Distribution(format!("decrypted password is not UTF-8: {e}")))?;
        Ok(Self {
            creds: BasicCreds { username, password },
        })
    }
}

#[async_trait]
impl AuthStrategy for StaticStrategy {
    async fn fetch_creds(&self) -> Result<BasicCreds, OciError> {
        Ok(self.creds.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_crypto::{CredCipher, EnvVarKeyProvider};

    #[tokio::test]
    async fn seal_open_round_trips_through_strategy() {
        let kek = EnvVarKeyProvider::from_bytes([42u8; 32], "test:v1");
        let sealed = CredCipher::new(&kek).seal(b"sekret").await.unwrap();
        let strategy = StaticStrategy::seal_open(
            &kek,
            "_json_key".into(),
            &sealed.wrapped_dek,
            &sealed.nonce,
            &sealed.ciphertext,
            &sealed.key_id,
        )
        .await
        .unwrap();
        let creds = strategy.fetch_creds().await.unwrap();
        assert_eq!(creds.username, "_json_key");
        assert_eq!(creds.password, "sekret");
    }

    #[tokio::test]
    async fn nonce_with_wrong_length_errors_at_build_time() {
        let kek = EnvVarKeyProvider::from_bytes([0u8; 32], "test:v1");
        let res = StaticStrategy::seal_open(
            &kek,
            "u".into(),
            &[0; 32],
            &[0; 8], // too short
            &[0; 32],
            "test:v1",
        )
        .await;
        assert!(matches!(res, Err(OciError::Distribution(_))));
    }

    #[tokio::test]
    async fn wrong_kek_fails_at_build_time_not_per_pull() {
        let kek_seal = EnvVarKeyProvider::from_bytes([1u8; 32], "a");
        let kek_open = EnvVarKeyProvider::from_bytes([2u8; 32], "b");
        let sealed = CredCipher::new(&kek_seal).seal(b"x").await.unwrap();
        let res = StaticStrategy::seal_open(
            &kek_open,
            "u".into(),
            &sealed.wrapped_dek,
            &sealed.nonce,
            &sealed.ciphertext,
            &sealed.key_id,
        )
        .await;
        assert!(matches!(res, Err(OciError::Distribution(_))));
    }
}
