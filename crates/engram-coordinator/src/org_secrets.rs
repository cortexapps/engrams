//! ADR 0057: the org-secret store — read backend + seal/open helpers.
//!
//! Org secrets are admin-entered, KEK-sealed credential values stored in the
//! coordinator's Postgres (`org_secrets` table, via `MetadataStore`). They are
//! the single value store for profile secrets, integration inject credentials,
//! and the GitHub App mint key.
//!
//! [`OrgSecretBackend`] is the **read** side: a [`SecretStore`] over `meta` +
//! `kek`, layered ahead of the deployment backend (env / GCP SM) so an
//! admin-entered org secret resolves before any image/manifest secret. The
//! image `SecretContext` is ignored — org secrets are global, name-keyed.
//!
//! [`seal_org_secret`] is the **write** side helper: it seals a plaintext at
//! the coordinator (the value never transits the orchestrator wire) into a
//! [`SealedOrgSecret`] the `OrgSecretService` upserts via `meta`.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::{MetadataStore, SecretContext, SecretStore};
use engram_core::types::org_secret::SealedOrgSecret;
use engram_core::types::SecretSchema;
use engram_core::SecretError;
use engram_crypto::{CredCipher, CryptoError, MasterKeyProvider, SealedCred};

/// A [`SecretStore`] resolving admin-managed org secrets from the coordinator
/// Postgres, unsealing under the KEK. Name-keyed; composed first in the
/// `LayeredSecretStore` (org store wins, deployment backend is the fallback).
pub struct OrgSecretBackend {
    meta: Arc<dyn MetadataStore>,
    kek: Arc<dyn MasterKeyProvider>,
}

impl OrgSecretBackend {
    pub fn new(meta: Arc<dyn MetadataStore>, kek: Arc<dyn MasterKeyProvider>) -> Self {
        Self { meta, kek }
    }
}

#[async_trait]
impl SecretStore for OrgSecretBackend {
    async fn get(
        &self,
        _ctx: &SecretContext<'_>,
        name: &str,
        _schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        let Some(sealed) = self
            .meta
            .get_org_secret_sealed(name)
            .await
            .map_err(|e| SecretError::Backend(Box::new(e)))?
        else {
            return Ok(None);
        };
        let cred = to_sealed_cred(&sealed)?;
        let plaintext = CredCipher::new(&*self.kek)
            .open(&cred)
            .await
            .map_err(|e| SecretError::Backend(Box::new(e)))?;
        let value = String::from_utf8(plaintext)
            .map_err(|_| SecretError::BadValue(format!("org secret `{name}` is not UTF-8")))?;
        Ok(Some(value))
    }
}

/// Seal a plaintext org-secret value under the KEK (the write path). The
/// coordinator seals on write so the orchestrator/web never persist the value.
pub async fn seal_org_secret(
    kek: &dyn MasterKeyProvider,
    name: &str,
    value: &[u8],
) -> Result<SealedOrgSecret, CryptoError> {
    let sealed: SealedCred = CredCipher::new(kek).seal(value).await?;
    Ok(SealedOrgSecret {
        name: name.to_string(),
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
    })
}

/// Rebuild a [`SealedCred`] from a stored [`SealedOrgSecret`]. The DB stores the
/// nonce as `bytea`; a length other than 12 means a corrupted row.
fn to_sealed_cred(sealed: &SealedOrgSecret) -> Result<SealedCred, SecretError> {
    let nonce: [u8; 12] = sealed.nonce.as_slice().try_into().map_err(|_| {
        SecretError::BadValue(format!(
            "org secret `{}` has a {}-byte nonce (expected 12)",
            sealed.name,
            sealed.nonce.len()
        ))
    })?;
    Ok(SealedCred {
        wrapped_dek: sealed.wrapped_dek.clone(),
        nonce,
        ciphertext: sealed.ciphertext.clone(),
        key_id: sealed.key_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_crypto::EnvVarKeyProvider;

    #[tokio::test]
    async fn seal_then_open_round_trips() {
        let kek = EnvVarKeyProvider::from_bytes([7u8; 32], "test:v1");
        let sealed = seal_org_secret(&kek, "datadog-api-key", b"dd-secret-value")
            .await
            .unwrap();
        assert_eq!(sealed.name, "datadog-api-key");
        assert_eq!(sealed.key_id, "test:v1");
        assert_eq!(sealed.nonce.len(), 12);
        assert_ne!(
            sealed.ciphertext, b"dd-secret-value",
            "ciphertext must not equal plaintext"
        );

        // The read path: rebuild the SealedCred from the stored row + open
        // under the same KEK — what `OrgSecretBackend::get` does after the
        // `meta` lookup.
        let cred = to_sealed_cred(&sealed).unwrap();
        let plaintext = CredCipher::new(&kek).open(&cred).await.unwrap();
        assert_eq!(plaintext, b"dd-secret-value");
    }

    #[tokio::test]
    async fn wrong_kek_cannot_open() {
        let kek_a = EnvVarKeyProvider::from_bytes([0xaa; 32], "a:v1");
        let kek_b = EnvVarKeyProvider::from_bytes([0xbb; 32], "b:v1");
        let sealed = seal_org_secret(&kek_a, "k", b"v").await.unwrap();
        let cred = to_sealed_cred(&sealed).unwrap();
        assert!(CredCipher::new(&kek_b).open(&cred).await.is_err());
    }

    #[test]
    fn corrupt_nonce_length_is_a_bad_value() {
        let bad = SealedOrgSecret {
            name: "x".into(),
            wrapped_dek: vec![0; 40],
            nonce: vec![0; 8], // not 12 → corrupt row
            ciphertext: vec![0; 16],
            key_id: "test:v1".into(),
        };
        assert!(matches!(
            to_sealed_cred(&bad),
            Err(SecretError::BadValue(_))
        ));
    }
}
