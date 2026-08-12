//! KEK provider implementations.
//!
//! - [`EnvVarKeyProvider`]: reads a 32-byte master key from a process
//!   env var (base64-encoded). Used for dev (`just bootstrap` writes
//!   the key into `.env`) and for prod deployments that source secrets
//!   from k8s Secrets / Doppler / Vault into env.
//! - [`GcpKmsProvider`]: stub for prod KMS-backed wrap/unwrap. Wired
//!   when needed; placeholder today, mirroring `engram-secrets-gcp`.

use async_trait::async_trait;
use base64::Engine;

use crate::{CryptoError, MasterKeyProvider};

/// 32-byte AES-256 key sourced from a process env var (base64).
///
/// The wrap format is the simplest envelope: AES-256-GCM encrypt the
/// DEK with a deterministic nonce derived from the DEK plaintext is
/// **not** safe — instead we use a random 12-byte nonce per wrap and
/// prepend it to the wrapped output. So `wrapped_dek` is laid out as:
///
/// ```text
///   [0..12]   nonce_for_dek
///   [12..]    AES-256-GCM(KEK, nonce_for_dek, DEK_plaintext)
/// ```
///
/// The same provider unwrap reads the prefix, runs AES-GCM open, and
/// returns the plaintext DEK. Tag validation guarantees that wrapped
/// values from a different KEK fail to unwrap.
pub struct EnvVarKeyProvider {
    key_bytes: [u8; 32],
    key_id: String,
}

impl EnvVarKeyProvider {
    /// Construct from a base64-encoded value (whatever the env var
    /// holds). Padding-tolerant. Errors if the decoded length is not
    /// 32 bytes.
    pub fn from_base64(b64: &str, key_id: impl Into<String>) -> Result<Self, CryptoError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64.trim()))
            .map_err(|e| CryptoError::Provider(format!("base64 decode: {e}")))?;
        if bytes.len() != 32 {
            return Err(CryptoError::Provider(format!(
                "KEK must be 32 bytes after base64 decode, got {}",
                bytes.len()
            )));
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&bytes);
        Ok(Self {
            key_bytes,
            key_id: key_id.into(),
        })
    }

    /// Read the env var named `var_name`, decode it, and build a
    /// provider. Fails closed if the var is missing, empty, or not
    /// 32 bytes.
    pub fn from_env(var_name: &str) -> Result<Self, CryptoError> {
        let raw = std::env::var(var_name).map_err(|_| {
            CryptoError::Provider(format!(
                "required env var `{var_name}` is unset; \
                 generate a 32-byte key (base64) and export it before starting the coordinator"
            ))
        })?;
        let key_id = format!("env:{var_name}:v1");
        Self::from_base64(&raw, key_id)
    }

    /// For tests only. Skip the env-var dance.
    pub fn from_bytes(key_bytes: [u8; 32], key_id: impl Into<String>) -> Self {
        Self {
            key_bytes,
            key_id: key_id.into(),
        }
    }

    fn cipher(&self) -> aes_gcm::Aes256Gcm {
        use aes_gcm::KeyInit;
        aes_gcm::Aes256Gcm::new((&self.key_bytes).into())
    }
}

#[async_trait]
impl MasterKeyProvider for EnvVarKeyProvider {
    async fn wrap(&self, dek: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use aes_gcm::aead::Aead;
        use rand::rngs::OsRng;
        use rand::RngCore;

        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = self.cipher();
        let mut out = cipher
            .encrypt((&nonce).into(), dek)
            .map_err(|_| CryptoError::Aead("wrap"))?;
        // Prepend nonce.
        let mut wrapped = Vec::with_capacity(12 + out.len());
        wrapped.extend_from_slice(&nonce);
        wrapped.append(&mut out);
        Ok(wrapped)
    }

    async fn unwrap(&self, wrapped: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use aes_gcm::aead::Aead;

        // 12-byte nonce + at least the 16-byte GCM tag. Splitting off a
        // fixed-size chunk gives the nonce a compile-time length, so the
        // cipher takes it without a fallible conversion.
        let Some((nonce, ct)) = wrapped
            .split_first_chunk::<12>()
            .filter(|(_, ct)| ct.len() >= 16)
        else {
            return Err(CryptoError::Provider(format!(
                "wrapped DEK too short ({} bytes)",
                wrapped.len()
            )));
        };
        let cipher = self.cipher();
        cipher
            .decrypt(nonce.into(), ct)
            .map_err(|_| CryptoError::Aead("unwrap"))
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }
}

/// Stub for GCP KMS-backed wrap/unwrap. The real implementation calls
/// `projects.locations.keyRings.cryptoKeys.encrypt` and `.decrypt`
/// against a customer-managed key. Wired when needed; today returns
/// errors, mirroring the `engram-secrets-gcp` stub pattern.
pub struct GcpKmsProvider {
    key_id: String,
}

impl GcpKmsProvider {
    /// `key_resource` is the full KMS key path, e.g.
    /// `projects/my-proj/locations/global/keyRings/engram/cryptoKeys/kek`.
    pub fn new(key_resource: impl Into<String>) -> Result<Self, CryptoError> {
        let key_resource = key_resource.into();
        if key_resource.is_empty() {
            return Err(CryptoError::Provider("empty GCP KMS resource path".into()));
        }
        let key_id = format!("gcp-kms:{key_resource}");
        Ok(Self { key_id })
    }
}

#[async_trait]
impl MasterKeyProvider for GcpKmsProvider {
    async fn wrap(&self, _dek: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::Provider(
            "GcpKmsProvider::wrap is not implemented yet — \
             call google-cloud-kms `Encrypt` against {key_resource}"
                .into(),
        ))
    }

    async fn unwrap(&self, _wrapped: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::Provider(
            "GcpKmsProvider::unwrap is not implemented yet — \
             call google-cloud-kms `Decrypt` against {key_resource}"
                .into(),
        ))
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_env_fails_when_var_missing() {
        // Use a name extremely unlikely to be set.
        let res = EnvVarKeyProvider::from_env("ENGRAM_TEST_NONEXISTENT_KEK_X29F");
        assert!(matches!(res, Err(CryptoError::Provider(_))));
    }

    #[tokio::test]
    async fn from_base64_rejects_short_keys() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(matches!(
            EnvVarKeyProvider::from_base64(&short, "x"),
            Err(CryptoError::Provider(_))
        ));
    }

    #[tokio::test]
    async fn from_base64_accepts_valid_32_byte_key() {
        let key = base64::engine::general_purpose::STANDARD.encode([0xab; 32]);
        let p = EnvVarKeyProvider::from_base64(&key, "test:v1").unwrap();
        assert_eq!(p.key_id(), "test:v1");
    }

    /// KNOWN-ANSWER test: a stored KEK still decodes to the same key.
    ///
    /// Every other test here is self-consistent — it encodes bytes and decodes
    /// them back with the same `base64` — so an alphabet or padding change
    /// would keep them all green while silently turning every KEK already in a
    /// secret manager into a different 32 bytes. Sealed data would then fail to
    /// unwrap in production, not in CI.
    ///
    /// The literal is RFC 4648 standard base64 of the bytes 0x00..=0x1f. It is
    /// pinned to a literal, so a `base64` bump that moved the decoding fails
    /// here instead. If this fails, that is a re-key migration, not a new
    /// expected value.
    #[tokio::test]
    async fn stored_kek_base64_decodes_to_the_same_key() {
        const STORED: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        let expected: [u8; 32] = std::array::from_fn(|i| i as u8);

        // Decoding is not observable directly (the key bytes are private), so
        // prove it by key agreement: a provider built from the stored string
        // must unwrap what a provider built from the literal bytes wrapped.
        let from_stored = EnvVarKeyProvider::from_base64(STORED, "kat:v1").unwrap();
        let from_literal = EnvVarKeyProvider::from_bytes(expected, "kat:v1");

        let dek = [9u8; 32];
        let wrapped = from_literal.wrap(&dek).await.unwrap();
        assert_eq!(
            from_stored.unwrap(&wrapped).await.unwrap(),
            dek,
            "base64 decoding of a stored KEK moved — sealed data would not unwrap"
        );

        // The unpadded spelling is accepted too, and yields the same key.
        let unpadded = EnvVarKeyProvider::from_base64(STORED.trim_end_matches('='), "kat:v1")
            .expect("unpadded KEK must still decode");
        assert_eq!(unpadded.unwrap(&wrapped).await.unwrap(), dek);
    }

    #[tokio::test]
    async fn wrap_unwrap_round_trip_recovers_dek() {
        let p = EnvVarKeyProvider::from_bytes([42u8; 32], "test:v1");
        let dek = [7u8; 32];
        let wrapped = p.wrap(&dek).await.unwrap();
        // Wrapped value carries nonce(12) + ciphertext + tag(16) = >= 60 bytes.
        assert!(wrapped.len() >= 12 + 32 + 16);
        let recovered = p.unwrap(&wrapped).await.unwrap();
        assert_eq!(recovered, dek);
    }

    #[tokio::test]
    async fn gcp_kms_provider_stub_returns_provider_error() {
        let p = GcpKmsProvider::new("projects/x/locations/global/keyRings/r/cryptoKeys/k").unwrap();
        assert!(matches!(
            p.wrap(&[0u8; 32]).await,
            Err(CryptoError::Provider(_))
        ));
    }
}
